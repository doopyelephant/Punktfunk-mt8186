//! The glass budgets: how many presents may be undisplayed at once, and when a closed budget
//! reopens.
//!
//!  * [`TimelineBudget`] (SurfaceView): one release in flight, reopened at the targeted
//!    timeline's predicted latch, bounded by the `OnFrameRendered` confirms, with the submit
//!    margin it adapts from the measured latch.
//!  * [`AscBudget`] (ASurfaceControl): one transaction in flight until the slot clock is phased,
//!    then two, each reopened by its completion.
//!
//! Both force-open [`STALE_REOPEN_NS`] after a present whose reopen never comes, counted in
//! `forced`: 0 on a healthy device (Apple's `PresentGate.staleAfter`, same value).

use super::store::{Held, Store};
use super::{Counters, FALLBACK_PERIOD_NS};

/// A budget whose reopen never comes (clock glitch, mode switch, a lost completion) force-opens
/// this long after the present.
pub(crate) const STALE_REOPEN_NS: i64 = 100_000_000;

/// The ceiling of the SurfaceView submit margin ahead of a timeline's EXPECTED PRESENT, which is
/// SurfaceFlinger's own latch lead: a released buffer must be queued by SF's wakeup for that
/// vsync, so a present closer than the margin is missed and the next one targeted. Not the
/// timeline's `deadline`, which budgets GPU work a video buffer does not do.
///
/// 2.5 ms: SF latches ~1-2 ms before present and the release is a sub-ms binder call. Each ms of
/// margin is a ms on every frame's display stage, so the margin starts at 0 and widens only on
/// a measured late latch ([`TimelineBudget::adapt_margin`]).
const LATCH_MARGIN_NS: i64 = 2_500_000;

/// One widening step of the SurfaceView submit margin.
const MARGIN_STEP_NS: i64 = 500_000;

/// Latch samples a second needs before its p50 may widen the margin.
const MARGIN_MIN_SAMPLES: usize = 8;

/// Releases still unconfirmed by `OnFrameRendered` at which the SurfaceView budget stops handing
/// SurfaceFlinger more work.
///
/// The reopen is a prediction off the learned panel grid. A grid finer than the panel reopens the
/// budget before the display consumed anything: the BufferQueue fills, the codec runs out of
/// output buffers, the decoder stalls and the no-output backstop begs for keyframes. The confirms
/// are what reached glass, so they bound the prediction.
///
/// Six, not one: the platform may batch the callbacks and they trail a release by a vsync or two,
/// so a healthy device sits at 1-3 outstanding, and a held frame in the newest-wins slot is a
/// dropped frame the moment a fresher one decodes. This is the "structurally wrong" rail, not a
/// pacing knob; a presenter out-running its display climbs past it within a second. A
/// BufferQueue shallower than six never engages it, and the no-output backstop covers that.
const UNDISPLAYED_CAP: i32 = 6;

/// The SurfaceView glass budget: at most one undisplayed release, reopened at SurfaceFlinger's
/// latch for the timeline it targeted.
pub(crate) struct TimelineBudget {
    inflight: Option<InFlight>,
    /// When the unconfirmed-release rail engaged, so it can be forgiven if the confirms simply
    /// stopped coming. `None` while the rail is down.
    backed_up_since: Option<i64>,
    /// A vsync arrived since the last release: the retry beat for a parked frame, and what
    /// [`Counters::dry`] counts against.
    vsync_tick: bool,
    /// The live submit margin: from 0, widened by [`TimelineBudget::adapt_margin`].
    margin_ns: i64,
    /// `debug.punktfunk.latch_margin_us` set it; it never adapts.
    margin_pinned: bool,
}

/// The one release in flight.
struct InFlight {
    /// The release target's expected present minus the margin, or `release + period` without a
    /// timeline.
    reopen_at_ns: i64,
    released_at_ns: i64,
}

/// What one SurfaceView pump may release.
pub(crate) struct Pick<F> {
    pub(crate) frame: Option<Held<F>>,
    /// The rail wrote off the unconfirmed releases: the sink clears its confirm ledger.
    pub(crate) forgive: bool,
}

impl TimelineBudget {
    /// `pinned_margin_ns`: the sysprop's pin; `None` starts adaptive at 0.
    pub(crate) fn new(pinned_margin_ns: Option<i64>) -> TimelineBudget {
        TimelineBudget {
            inflight: None,
            backed_up_since: None,
            vsync_tick: false,
            margin_ns: pinned_margin_ns.unwrap_or(0),
            margin_pinned: pinned_margin_ns.is_some(),
        }
    }

    pub(crate) fn margin_ns(&self) -> i64 {
        self.margin_ns
    }

    pub(crate) fn margin_pinned(&self) -> bool {
        self.margin_pinned
    }

    /// A vsync pulse: the retry tick for a parked frame. Under cadence targeting the due time,
    /// not the tick, releases a frame.
    pub(crate) fn on_vsync(&mut self) {
        self.vsync_tick = true;
    }

    /// The present decision, run on every loop pass. Reopens on the predicted latch or the stale
    /// backstop, runs the unconfirmed-release rail (`unconfirmed` releases outstanding;
    /// `confirms_live`: this device delivers confirms at all), then takes the frame the store
    /// may release by its slot's reach. A closed budget parks it back. `grid_period_ns` is the
    /// grid a release snaps to.
    pub(crate) fn pick<F>(
        &mut self,
        store: &mut Store<F>,
        now_ns: i64,
        grid_period_ns: i64,
        unconfirmed: i32,
        confirms_live: bool,
        c: &mut Counters,
    ) -> Pick<F> {
        if let Some(f) = &self.inflight {
            if now_ns >= f.reopen_at_ns {
                self.inflight = None;
            } else if now_ns - f.released_at_ns > STALE_REOPEN_NS {
                c.forced += 1;
                self.inflight = None;
            }
        }
        // Evaluated whether or not a frame waits, so its forgiveness timer measures real time.
        let (backlogged, forgive) = self.rail(unconfirmed, confirms_live, now_ns, c);
        let none = Pick {
            frame: None,
            forgive,
        };
        // The FIFO releases on the due time, never one frame per tick: at 60 fps on 120 Hz that
        // would drain at twice supply and judder 1,3,1,3 (design §4.3, D3).
        if !store.newest_wins() && !store.head_is_releasable(self.reach_ns(now_ns, grid_period_ns))
        {
            if self.vsync_tick && store.is_empty() {
                c.dry += 1;
            }
            self.vsync_tick = false;
            return none;
        }
        let Some(held) = store.take() else {
            return none;
        };
        if self.inflight.is_some() || backlogged {
            if backlogged {
                c.queue_waits += 1;
            }
            c.no_budget += 1;
            store.park(held);
            return none;
        }
        self.vsync_tick = false;
        Pick {
            frame: Some(held),
            forgive,
        }
    }

    /// The measured rail beneath the predicted reopen ([`UNDISPLAYED_CAP`]): `(held back,
    /// forgiven)`. Stuck for [`STALE_REOPEN_NS`] means the ledger itself is unreliable (callbacks
    /// dropped under load, or SurfaceFlinger discarded the buffers): it is written off, counted
    /// in `forced`, rather than allowed to wedge the stream.
    fn rail(
        &mut self,
        unconfirmed: i32,
        confirms_live: bool,
        now_ns: i64,
        c: &mut Counters,
    ) -> (bool, bool) {
        if !confirms_live || unconfirmed < UNDISPLAYED_CAP {
            self.backed_up_since = None;
            return (false, false);
        }
        match self.backed_up_since {
            Some(t) if now_ns - t > STALE_REOPEN_NS => {
                self.backed_up_since = None;
                c.forced += 1;
                (false, true)
            }
            _ => {
                self.backed_up_since.get_or_insert(now_ns);
                (true, false)
            }
        }
    }

    /// The earliest present a release may target: the latch lead ahead of now, and never before
    /// the frame's due time. The due time moves the target; a late one does not drag it back.
    pub(crate) fn not_before_ns(&self, now_ns: i64, due_ns: Option<i64>) -> i64 {
        let submit_floor_ns = now_ns + self.margin_ns;
        due_ns.map_or(submit_floor_ns, |due| due.max(submit_floor_ns))
    }

    /// How far ahead a FIFO head may be due and still leave now: one grid period past the submit
    /// margin. Sooner buys nothing, since the release is timed either way; later risks the loop's
    /// 5 ms housekeeping wake landing inside the submit lead, which costs a whole refresh.
    pub(crate) fn reach_ns(&self, now_ns: i64, grid_period_ns: i64) -> i64 {
        now_ns + self.margin_ns + grid_period_ns
    }

    /// One release handed to SurfaceFlinger at `now_ns`. The budget reopens at SF's latch for the
    /// targeted vsync, `expected_present_ns − margin`, when the slot frees for the next refresh:
    /// not the `deadline` (it can already be past, piling two releases on one vsync) and not
    /// the present (a period late, capping the rate near half the panel). Without a timeline it
    /// reopens one measured `period_ns` after the release.
    pub(crate) fn released(
        &mut self,
        now_ns: i64,
        expected_present_ns: Option<i64>,
        period_ns: Option<i64>,
        c: &mut Counters,
    ) {
        let reopen_at_ns = expected_present_ns
            .map(|e| e - self.margin_ns)
            .unwrap_or(now_ns + period_ns.unwrap_or(FALLBACK_PERIOD_NS));
        self.inflight = Some(InFlight {
            reopen_at_ns,
            released_at_ns: now_ns,
        });
        c.released += 1;
    }

    /// The teardown: nothing is in flight once the held buffers are back.
    pub(crate) fn clear(&mut self) {
        self.inflight = None;
    }

    /// One second's measured latch p50 against the panel period. A release targets the first
    /// grid point past `now + margin`, so a frame that makes its vsync is on glass within one
    /// panel period of that margin; a p50 beyond it means SurfaceFlinger wanted more lead. Widens
    /// one step toward [`LATCH_MARGIN_NS`]; one-way, since a margin that proved necessary is
    /// never re-gambled mid-stream. Store evictions say nothing about the latch lead and never
    /// feed this. Returns whether the margin widened.
    pub(crate) fn adapt_margin(
        &mut self,
        latch_p50_ns: i64,
        latch_samples: usize,
        panel_ns: i64,
    ) -> bool {
        let late = panel_ns > 0
            && latch_samples >= MARGIN_MIN_SAMPLES
            && latch_p50_ns > panel_ns + self.margin_ns;
        if self.margin_pinned || self.margin_ns >= LATCH_MARGIN_NS || !late {
            return false;
        }
        self.margin_ns = (self.margin_ns + MARGIN_STEP_NS).min(LATCH_MARGIN_NS);
        true
    }
}

/// The ASurfaceControl glass budget: transactions applied and not yet completed. One until the
/// slot clock is phased, then two: distinct slot targets cannot collide, and SurfaceFlinger holds
/// the second.
#[derive(Default)]
pub(crate) struct AscBudget {
    inflight: u32,
}

impl AscBudget {
    pub(crate) fn inflight(&self) -> u32 {
        self.inflight
    }

    /// The frame one ASC pump presents, if any. `bootstrapping`: pacing wants slots and no
    /// present has phased the clock yet. `releases`: apply instants of the presents still on
    /// SurfaceFlinger, oldest first; the oldest in flight past [`STALE_REOPEN_NS`] force-opens
    /// the budget, as does a count with no present behind it. `reach_ns`: the instant a FIFO
    /// head must be due by.
    pub(crate) fn pick<F>(
        &mut self,
        store: &mut Store<F>,
        now_ns: i64,
        bootstrapping: bool,
        releases: impl DoubleEndedIterator<Item = i64>,
        reach_ns: i64,
        c: &mut Counters,
    ) -> Option<Held<F>> {
        let cap = if bootstrapping { 1 } else { 2 };
        if self.inflight >= cap {
            let stale = releases
                .rev()
                .nth(self.inflight as usize - 1)
                .is_none_or(|t| now_ns - t > STALE_REOPEN_NS);
            if !stale {
                return None;
            }
            c.forced += 1;
            self.inflight = 0;
        }
        if !store.head_is_releasable(reach_ns) {
            return None;
        }
        store.take()
    }

    /// One transaction applied.
    pub(crate) fn applied(&mut self, c: &mut Counters) {
        self.inflight += 1;
        c.released += 1;
    }

    /// One transaction completed.
    pub(crate) fn completed(&mut self) {
        self.inflight = self.inflight.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{frame_interval_ns, PresentPriority};
    use super::*;

    const P: i64 = 8_333_333;
    const T: i64 = 987_000_000_000;

    fn latency() -> Store<u32> {
        Store::new(PresentPriority::Latency, frame_interval_ns(120))
    }

    fn smooth() -> Store<u32> {
        Store::new(
            PresentPriority::Smooth { buffer: 2 },
            frame_interval_ns(120),
        )
    }

    fn frame_of<F>(p: Pick<F>) -> Option<F> {
        p.frame.map(|h| h.frame)
    }

    #[test]
    fn the_latency_floor_is_the_latch_lead_and_a_due_time_only_moves_it_later() {
        let b = TimelineBudget::new(None);
        assert_eq!(b.not_before_ns(T, None), T + b.margin_ns());
        let floor = T + b.margin_ns();
        assert_eq!(b.not_before_ns(T, Some(floor + P)), floor + P);
        // A late frame presents at the next opportunity; the grid is not dragged back to it.
        assert_eq!(b.not_before_ns(T, Some(floor - 5 * P)), floor);
        let pinned = TimelineBudget::new(Some(1_000_000));
        assert_eq!(pinned.reach_ns(T, P), T + 1_000_000 + P);
    }

    #[test]
    fn the_timeline_budget_reopens_at_the_targets_latch() {
        let margin = 1_000_000;
        let mut b = TimelineBudget::new(Some(margin));
        let mut s = latency();
        let mut c = Counters::default();
        s.push(1, 0, T, &mut c, drop);
        assert_eq!(frame_of(b.pick(&mut s, T, P, 0, true, &mut c)), Some(1));
        b.released(T, Some(T + 2 * P), Some(P), &mut c);
        s.push(2, 0, T, &mut c, drop);
        let reopen = T + 2 * P - margin;
        assert_eq!(
            frame_of(b.pick(&mut s, reopen - 1, P, 0, true, &mut c)),
            None
        );
        assert_eq!(c.no_budget, 1, "parked on the closed budget");
        assert_eq!(
            frame_of(b.pick(&mut s, reopen, P, 0, true, &mut c)),
            Some(2)
        );
        // No timeline: one measured period after the release, or one 120 Hz frame.
        b.released(reopen, None, Some(2 * P), &mut c);
        s.push(3, 0, T, &mut c, drop);
        assert_eq!(
            frame_of(b.pick(&mut s, reopen + 2 * P - 1, P, 0, true, &mut c)),
            None
        );
        assert_eq!(
            frame_of(b.pick(&mut s, reopen + 2 * P, P, 0, true, &mut c)),
            Some(3)
        );
        b.released(T, None, None, &mut c);
        s.push(4, 0, T, &mut c, drop);
        assert_eq!(
            frame_of(b.pick(&mut s, T + FALLBACK_PERIOD_NS, P, 0, true, &mut c)),
            Some(4)
        );
        assert_eq!((c.released, c.forced), (3, 0));
    }

    #[test]
    fn a_release_whose_reopen_never_comes_force_opens_after_100_ms() {
        let mut b = TimelineBudget::new(None);
        let mut s = latency();
        let mut c = Counters::default();
        b.released(T, Some(T + 10_000_000_000), Some(P), &mut c);
        s.push(1, 0, T, &mut c, drop);
        assert_eq!(
            frame_of(b.pick(&mut s, T + STALE_REOPEN_NS, P, 0, true, &mut c)),
            None
        );
        let p = b.pick(&mut s, T + STALE_REOPEN_NS + 1, P, 0, true, &mut c);
        assert_eq!(frame_of(p), Some(1));
        assert_eq!(c.forced, 1);
        // The teardown leaves nothing in flight.
        b.released(T, Some(T + 10_000_000_000), Some(P), &mut c);
        b.clear();
        s.push(2, 0, T, &mut c, drop);
        assert_eq!(frame_of(b.pick(&mut s, T + 1, P, 0, true, &mut c)), Some(2));
    }

    #[test]
    fn six_unconfirmed_releases_hold_a_release_until_forgiven_after_100_ms() {
        let mut b = TimelineBudget::new(None);
        let mut s = latency();
        let mut c = Counters::default();
        s.push(1, 0, T, &mut c, drop);
        let p = b.pick(&mut s, T, P, UNDISPLAYED_CAP, true, &mut c);
        assert!(p.frame.is_none() && !p.forgive);
        assert_eq!((c.queue_waits, c.no_budget), (1, 1));
        let p = b.pick(
            &mut s,
            T + STALE_REOPEN_NS,
            P,
            UNDISPLAYED_CAP,
            true,
            &mut c,
        );
        assert!(p.frame.is_none() && !p.forgive, "100 ms is still patience");
        let p = b.pick(
            &mut s,
            T + STALE_REOPEN_NS + 1,
            P,
            UNDISPLAYED_CAP,
            true,
            &mut c,
        );
        assert!(p.forgive, "a stuck ledger is written off");
        assert_eq!(frame_of(p), Some(1));
        assert_eq!((c.forced, c.queue_waits), (1, 2));
        // One under the cap, or a device without confirms: never held.
        for (unconfirmed, live) in [(UNDISPLAYED_CAP - 1, true), (60, false)] {
            let mut b = TimelineBudget::new(None);
            s.push(2, 0, T, &mut c, drop);
            assert_eq!(
                frame_of(b.pick(&mut s, T, P, unconfirmed, live, &mut c)),
                Some(2)
            );
        }
    }

    #[test]
    fn an_empty_smoothing_store_counts_a_dry_tick_and_a_latency_one_never_does() {
        let mut c = Counters::default();
        let mut b = TimelineBudget::new(None);
        let mut s = smooth();
        b.on_vsync();
        assert!(b.pick(&mut s, T, P, 0, true, &mut c).frame.is_none());
        assert!(b.pick(&mut s, T, P, 0, true, &mut c).frame.is_none());
        assert_eq!(c.dry, 1, "the tick was spent on the first pass");
        let mut s = latency();
        b.on_vsync();
        assert!(b.pick(&mut s, T, P, 0, true, &mut c).frame.is_none());
        assert_eq!(c.dry, 1);
    }

    #[test]
    fn the_margin_widens_a_step_per_late_second_up_to_the_ceiling_and_never_narrows() {
        let panel = P;
        let mut b = TimelineBudget::new(None);
        assert!(!b.margin_pinned());
        assert_eq!(b.margin_ns(), 0);
        assert!(
            !b.adapt_margin(panel, 60, panel),
            "on the panel period is on time"
        );
        assert!(!b.adapt_margin(2 * panel, 7, panel), "too few samples");
        assert!(!b.adapt_margin(2 * panel, 60, 0), "no panel period known");
        for step in 1..=5 {
            let late = panel + b.margin_ns() + 1;
            assert!(b.adapt_margin(late, 60, panel));
            assert_eq!(b.margin_ns(), step * MARGIN_STEP_NS);
        }
        assert_eq!(b.margin_ns(), LATCH_MARGIN_NS);
        assert!(!b.adapt_margin(10 * panel, 60, panel), "capped");
        assert!(!b.adapt_margin(0, 60, panel));
        assert_eq!(b.margin_ns(), LATCH_MARGIN_NS, "never narrows");
        let mut pinned = TimelineBudget::new(Some(0));
        assert!(pinned.margin_pinned());
        assert!(!pinned.adapt_margin(10 * panel, 60, panel));
        assert_eq!(pinned.margin_ns(), 0);
    }

    #[test]
    fn the_asc_budget_holds_one_until_phased_then_two() {
        let mut b = AscBudget::default();
        let mut s = latency();
        let mut c = Counters::default();
        let mut releases: Vec<i64> = Vec::new();
        s.push(1, 0, T, &mut c, drop);
        let f = b.pick(&mut s, T, true, releases.iter().copied(), T, &mut c);
        assert_eq!(f.map(|h| h.frame), Some(1));
        b.applied(&mut c);
        releases.push(T);
        s.push(2, 0, T, &mut c, drop);
        let now = T + 1_000_000;
        let f = b.pick(&mut s, now, true, releases.iter().copied(), now, &mut c);
        assert!(f.is_none(), "one in flight before the first fence");
        let f = b.pick(&mut s, now, false, releases.iter().copied(), now, &mut c);
        assert_eq!(f.map(|h| h.frame), Some(2), "two once phased");
        b.applied(&mut c);
        releases.push(now);
        s.push(3, 0, T, &mut c, drop);
        let f = b.pick(&mut s, now, false, releases.iter().copied(), now, &mut c);
        assert!(f.is_none());
        b.completed();
        let f = b.pick(&mut s, now, false, releases.iter().copied(), now, &mut c);
        assert_eq!(f.map(|h| h.frame), Some(3));
        assert_eq!((b.inflight(), c.released, c.forced), (1, 2, 0));
    }

    #[test]
    fn a_completion_that_never_comes_force_opens_after_100_ms() {
        let mut b = AscBudget::default();
        let mut s = latency();
        let mut c = Counters::default();
        b.applied(&mut c);
        let releases = [T];
        s.push(1, 0, T, &mut c, drop);
        let late = T + STALE_REOPEN_NS;
        assert!(b
            .pick(&mut s, late, true, releases.into_iter(), late, &mut c)
            .is_none());
        let f = b.pick(&mut s, late + 1, true, releases.into_iter(), late, &mut c);
        assert_eq!(f.map(|h| h.frame), Some(1));
        assert_eq!((c.forced, b.inflight()), (1, 0));
        // A count with no present behind it is bookkeeping drift and reopens too.
        b.applied(&mut c);
        s.push(2, 0, T, &mut c, drop);
        let f = b.pick(&mut s, T, true, std::iter::empty(), T, &mut c);
        assert_eq!(f.map(|h| h.frame), Some(2));
        assert_eq!(c.forced, 2);
    }

    #[test]
    fn the_asc_fifo_waits_for_a_head_due_within_reach() {
        let mut b = AscBudget::default();
        let mut s = smooth();
        let mut c = Counters::default();
        s.push(1, 0, T, &mut c, drop);
        let head = s.take().expect("held");
        let due = head.due_ns.expect("smooth frames carry a due time");
        s.park(head);
        assert!(b
            .pick(&mut s, T, false, std::iter::empty(), due - 1, &mut c)
            .is_none());
        let f = b.pick(&mut s, T, false, std::iter::empty(), due, &mut c);
        assert_eq!(f.map(|h| h.frame), Some(1));
    }
}
