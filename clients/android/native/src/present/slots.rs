//! The ASurfaceControl slot scheduler: each frame its own vsync slot on the present-phased grid.
//!
//! SurfaceFlinger defers a transaction only while `desiredPresentTime >= expectedPresentTime`, so
//! two frames applied in one period with targets before the next vsync latch together and only
//! the newer shows. Every frame therefore gets its own slot ([`pace_slot`]) and targets
//! `present − period/2`: ready for that slot, not the one before, half a period of tolerance each
//! way. Under `latency` a frame is held at most one slot beyond reachable, only when the hold
//! saves a drop; under `smooth` the due time is the earliest instant and the buffer bounds the
//! hold. Until a present fence phases the [`SlotClock`], or with pacing off, every target is as
//! soon as possible.

use punktfunk_core::phase::{pace_slot, SlotClock};

use super::{Counters, PresentPriority, FALLBACK_PERIOD_NS};
use crate::apply_margin::ApplyMargin;

/// The slot clock, the pacer's memory of the slots it assigned and observed, and the apply margin
/// the observed slots adapt.
pub(crate) struct Slots {
    clock: SlotClock,
    /// The last slot assigned or observed; the next frame never shares it.
    last_slot: Option<i64>,
    /// Slots a frame may be held beyond the first reachable one: 1 under latency, the buffer
    /// under smooth.
    max_ahead: i64,
    apply_margin: ApplyMargin,
    /// Off (`debug.punktfunk.asc_pacing` = 0): as-soon-as-possible targets, two pending.
    pacing: bool,
    /// Off (`debug.punktfunk.asc_hold_chain` = 0) under latency: a hold never follows a hold, the
    /// frame shares the held frame's slot and SurfaceFlinger keeps the newer. On: the depth-one
    /// elastic queue, which on an exact-rate source holds every frame after the first burst.
    hold_chain: bool,
    /// The store is newest-wins; only there may a hold be shed.
    newest_wins: bool,
    /// The last assigned slot was a hold.
    last_held: bool,
    /// A present fence has been read at least once this session.
    fence_live: bool,
    last_latch_ns: i64,
    last_present_ns: i64,
    last_observed_slot: Option<i64>,
}

impl Slots {
    /// `panel_hz` (the mode-table rate) seeds the clock; the present fences phase it.
    pub(crate) fn new(
        panel_hz: i32,
        priority: PresentPriority,
        pacing: bool,
        hold_chain: bool,
    ) -> Slots {
        let (max_ahead, newest_wins) = match priority {
            PresentPriority::Latency => (1, true),
            PresentPriority::Smooth { buffer } => ((buffer as i64).clamp(1, 3), false),
        };
        Slots {
            clock: SlotClock::seeded(panel_hz),
            last_slot: None,
            max_ahead,
            apply_margin: ApplyMargin::new(),
            pacing,
            hold_chain,
            newest_wins,
            last_held: false,
            fence_live: false,
            last_latch_ns: 0,
            last_present_ns: 0,
            last_observed_slot: None,
        }
    }

    /// Slot scheduling is live: pacing wanted and the clock has a real present behind it.
    fn paced(&self) -> bool {
        self.pacing && self.clock.phased()
    }

    /// Pacing wants slots and no present fence has phased the clock yet: the budget holds one.
    pub(crate) fn bootstrapping(&self) -> bool {
        self.pacing && !self.clock.phased()
    }

    /// The choreographer's panel period (0 = none yet): the compositor's own statement of the
    /// grid, which a source below the panel rate cannot bias the way present spacings can.
    pub(crate) fn set_period(&mut self, panel_period_ns: i64) {
        if panel_period_ns > 0 {
            self.clock.set_period(panel_period_ns);
        }
    }

    /// The panel period, or one 120 Hz frame while none has been learned.
    pub(crate) fn period_ns(&self) -> i64 {
        match self.clock.period_ns() {
            0 => FALLBACK_PERIOD_NS,
            p => p,
        }
    }

    /// The clock's own period, 0 while unknown, for the 1 Hz line.
    pub(crate) fn clock_period_ns(&self) -> i64 {
        self.clock.period_ns()
    }

    pub(crate) fn lead_ns(&self) -> i64 {
        self.clock.lead_ns()
    }

    pub(crate) fn margin_ns(&self) -> i64 {
        self.apply_margin.ns()
    }

    pub(crate) fn fence_live(&self) -> bool {
        self.fence_live
    }

    /// The desired present time for a frame whose earliest presentable instant is `earliest`
    /// (`CLOCK_MONOTONIC`), and the slot it aims at. `0` = as soon as possible: the bootstrap
    /// before the first present sample, a device without present fences, or pacing switched off.
    pub(crate) fn next_target(&mut self, earliest: i64, c: &mut Counters) -> (i64, Option<i64>) {
        if !self.paced() {
            return (0, None);
        }
        let reachable = self
            .clock
            .slot_after(earliest + self.clock.lead_ns() + self.apply_margin.ns());
        let mut slot = pace_slot(reachable, self.last_slot, self.max_ahead);
        let mut held = slot > reachable;
        if held && self.last_held && !self.hold_chain && self.newest_wins {
            // Share the held frame's slot: SurfaceFlinger keeps the newer, the stale held frame
            // drops (`coalesced` counts it).
            slot = self.last_slot.unwrap_or(reachable);
            held = false;
        }
        if held {
            c.held += 1;
        }
        self.last_held = held;
        self.last_slot = Some(slot);
        (
            self.clock.present_ns(slot) - self.clock.period_ns() / 2,
            Some(slot),
        )
    }

    /// One completion's latch (0 = none: `unlatched`). Two transactions latched in one commit
    /// share the instant and SurfaceFlinger showed only the newer; the interval histogram sees
    /// the same pair as a 0-slot spacing.
    pub(crate) fn note_latch(&mut self, latch_ns: i64, c: &mut Counters) {
        if latch_ns == 0 {
            c.unlatched += 1;
            return;
        }
        if latch_ns == self.last_latch_ns {
            c.coalesced += 1;
        }
        self.last_latch_ns = latch_ns;
    }

    /// One frame on glass at `present_ns`, aimed at `slot`. With a `real` vsync (a present
    /// fence) phase the clock and correct the pacer by what happened; a latch stands in where no
    /// fence exists and never phases the clock, so such a device keeps the one-pending ASAP
    /// budget. Scores the interval either way. Returns the phase error, µs, when there is one.
    pub(crate) fn on_present(
        &mut self,
        present_ns: i64,
        latch_ns: i64,
        slot: Option<i64>,
        real: bool,
        c: &mut Counters,
    ) -> Option<u64> {
        c.displays += 1;
        self.fence_live |= real;
        let mut phase_err_us = None;
        if present_ns == self.last_present_ns {
            c.intervals.note(0); // the coalesced pair's second half
        } else if present_ns > self.last_present_ns {
            if real {
                phase_err_us = self.observe_present(present_ns, latch_ns, slot, c);
            } else if self.last_present_ns > 0 {
                let p = self.period_ns();
                c.intervals
                    .note((present_ns - self.last_present_ns + p / 2).div_euclid(p));
            }
            self.last_present_ns = present_ns;
        }
        phase_err_us
    }

    /// A real vsync for a frame aimed at `assigned`: phase the clock, score the slot interval,
    /// and correct the pacer when the frame landed later than aimed.
    fn observe_present(
        &mut self,
        present_ns: i64,
        latch_ns: i64,
        assigned: Option<i64>,
        c: &mut Counters,
    ) -> Option<u64> {
        let phase_err_us = assigned
            .filter(|_| self.clock.phased())
            .map(|a| (present_ns - self.clock.present_ns(a)).unsigned_abs() / 1000);
        let observed = self.clock.observe(present_ns, latch_ns);
        if let Some(prev) = self.last_observed_slot {
            c.intervals.note(observed - prev);
        }
        self.last_observed_slot = Some(observed);
        // SurfaceFlinger woke before the apply landed, or the uid's frame-rate override skipped
        // that vsync: the next frame must not aim at the slot this one took.
        let missed = assigned.is_some_and(|s| observed > s);
        if missed {
            c.slot_miss += 1;
        }
        self.apply_margin.observe(missed);
        if self.last_slot.is_none_or(|l| observed > l) {
            self.last_slot = Some(observed);
        }
        phase_err_us
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: i64 = 8_333_333; // 120 Hz
    const LEAD: i64 = 4_000_000; // present − latch
    const T0: i64 = 1_000_000_000_000; // the first fenced present, monotonic

    /// A scheduler phased by one fenced present at `T0` (slot 0).
    fn phased(priority: PresentPriority, hold_chain: bool) -> (Slots, Counters) {
        let mut s = Slots::new(120, priority, true, hold_chain);
        let mut c = Counters::default();
        assert!(s.bootstrapping());
        assert_eq!(s.next_target(T0 - P, &mut c), (0, None));
        s.note_latch(T0 - LEAD, &mut c);
        assert_eq!(s.on_present(T0, T0 - LEAD, None, true, &mut c), None);
        assert!(!s.bootstrapping() && s.fence_live());
        (s, c)
    }

    /// Aim a frame presentable at `earliest`; its target is half a period before its slot.
    fn aim(s: &mut Slots, c: &mut Counters, earliest: i64) -> i64 {
        let (target, slot) = s.next_target(earliest, c);
        let slot = slot.expect("phased");
        assert_eq!(target, s.clock.present_ns(slot) - P / 2);
        slot
    }

    /// Land a frame aimed at `slot` on vsync `on`, fenced, as a device does.
    fn land(s: &mut Slots, c: &mut Counters, slot: i64, on: i64) -> Option<u64> {
        let present = s.clock.present_ns(on);
        s.note_latch(present - LEAD, c);
        s.on_present(present, present - LEAD, Some(slot), true, c)
    }

    #[test]
    fn quiet_120_on_120_gives_every_frame_its_own_slot_and_holds_none() {
        let (mut s, mut c) = phased(PresentPriority::Latency, true);
        for k in 0..120 {
            let slot = aim(&mut s, &mut c, T0 + k * P + P / 10);
            assert_eq!(slot, k + 1);
            assert_eq!(land(&mut s, &mut c, slot, slot), Some(0));
        }
        assert_eq!((c.held, c.slot_miss, c.coalesced), (0, 0, 0));
        assert_eq!((c.intervals.n0, c.intervals.n1), (0, 120));
        assert_eq!((c.intervals.mode(), c.intervals.judder_permille()), (1, 0));
        assert_eq!((c.displays, s.margin_ns(), s.lead_ns()), (121, 0, LEAD));
        assert_eq!((s.period_ns(), s.clock_period_ns()), (P, P));
    }

    #[test]
    fn two_frames_inside_one_period_hold_the_second_one_slot_without_coalescing() {
        let (mut s, mut c) = phased(PresentPriority::Latency, true);
        let a = aim(&mut s, &mut c, T0 + P / 10);
        let b = aim(&mut s, &mut c, T0 + P / 4);
        assert_eq!((a, b, c.held), (1, 2, 1));
        // A third while two are pending clamps to `max_ahead`: it re-targets the held slot, and
        // SurfaceFlinger shows the newer of the pair.
        let third = aim(&mut s, &mut c, T0 + P / 2);
        assert_eq!((third, c.held), (b, 2));
        land(&mut s, &mut c, a, a);
        land(&mut s, &mut c, b, b);
        assert_eq!(c.coalesced, 0);
        land(&mut s, &mut c, third, third);
        assert_eq!((c.coalesced, c.intervals.n0, c.intervals.n1), (1, 1, 2));
    }

    #[test]
    fn without_the_hold_chain_a_hold_never_follows_a_hold() {
        for (hold_chain, want_slot, want_held) in [(true, 3, 2), (false, 2, 1)] {
            let (mut s, mut c) = phased(PresentPriority::Latency, hold_chain);
            assert_eq!(aim(&mut s, &mut c, T0 + P / 10), 1);
            assert_eq!(aim(&mut s, &mut c, T0 + P / 4), 2, "the first hold");
            let next = aim(&mut s, &mut c, T0 + P + P / 4);
            assert_eq!((next, c.held), (want_slot, want_held), "chain {hold_chain}");
        }
    }

    #[test]
    fn an_unphased_clock_or_pacing_off_aims_as_soon_as_possible() {
        let mut c = Counters::default();
        let mut s = Slots::new(120, PresentPriority::Latency, true, true);
        // A latch stands in for a vsync but never phases the clock.
        s.on_present(T0, T0 - LEAD, None, false, &mut c);
        assert!(s.bootstrapping() && !s.fence_live());
        assert_eq!(s.next_target(T0, &mut c), (0, None));
        let mut off = Slots::new(120, PresentPriority::Latency, false, true);
        off.on_present(T0, T0 - LEAD, None, true, &mut c);
        assert!(
            !off.bootstrapping(),
            "pacing off: two pending from the start"
        );
        assert_eq!(off.next_target(T0, &mut c), (0, None));
        assert_eq!(c.held, 0);
        // No seed: the fallback period until the choreographer states one; 0 states nothing.
        let mut unseeded = Slots::new(0, PresentPriority::Latency, true, true);
        unseeded.set_period(0);
        assert_eq!(
            (unseeded.period_ns(), unseeded.clock_period_ns()),
            (FALLBACK_PERIOD_NS, 0)
        );
        unseeded.set_period(2 * P);
        assert_eq!(unseeded.period_ns(), 2 * P);
    }

    #[test]
    fn sixty_on_120_smooth_presents_every_second_vsync() {
        let (mut s, mut c) = phased(PresentPriority::Smooth { buffer: 2 }, true);
        for k in 0..60 {
            let slot = aim(&mut s, &mut c, T0 + (2 * k + 1) * P + P / 10);
            assert_eq!(slot, 2 * k + 2);
            land(&mut s, &mut c, slot, slot);
        }
        assert_eq!((c.held, c.intervals.n2, c.intervals.mode()), (0, 60, 2));
        assert_eq!(c.intervals.judder_permille(), 0);
    }

    #[test]
    fn a_frame_that_lands_a_slot_late_widens_the_margin_and_frees_its_slot() {
        let (mut s, mut c) = phased(PresentPriority::Latency, true);
        let a = aim(&mut s, &mut c, T0 + P / 10);
        assert_eq!(land(&mut s, &mut c, a, a + 1), Some((P / 1000) as u64));
        assert_eq!((c.slot_miss, s.margin_ns()), (1, 500_000));
        // Reachable is the slot the late frame took; the pacer moves past it.
        let next = aim(&mut s, &mut c, T0 + P + P / 10);
        assert_eq!(next, a + 2, "never the slot the late frame took");
    }

    #[test]
    fn latch_only_presents_score_the_spacing_off_the_period() {
        let mut c = Counters::default();
        let mut s = Slots::new(120, PresentPriority::Latency, true, true);
        s.note_latch(0, &mut c);
        assert_eq!(c.unlatched, 1);
        for k in [0, 1, 3, 3] {
            s.on_present(T0 + k * P, T0 + k * P - LEAD, None, false, &mut c);
        }
        let i = c.intervals;
        assert_eq!((i.n0, i.n1, i.n2, i.n3p), (1, 1, 1, 0));
    }
}
