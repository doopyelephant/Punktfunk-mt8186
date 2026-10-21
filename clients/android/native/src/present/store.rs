//! The held-frame store between decode and present, one per sink.
//!
//! Under `latency` it is a newest-wins slot: a fresher frame evicts the held one, an explicit,
//! counted drop, so a burst coalesces in the app instead of queueing behind the display. Under
//! `smooth` it is a 1–3 frame FIFO whose frames carry a due time from the source's own
//! timestamps ([`CadenceClock`]): the due time paces, the capacity only bounds a burst
//! (`design/presenter-cadence-rework.md` §4.3). The latency store holds no clock at all, so it
//! cannot drift into cadence targeting by accident.

use punktfunk_core::phase::{CadenceClock, CadenceHealth, CadenceTuning};
use std::collections::VecDeque;

use super::{Counters, PresentPriority};

/// One frame held for presentation.
pub(crate) struct Held<F> {
    pub(crate) frame: F,
    /// When the source says the frame is due (`CLOCK_MONOTONIC`); `None` under latency.
    pub(crate) due_ns: Option<i64>,
}

/// The held frames. `F` is the sink's handle: a codec buffer index or a reader image.
pub(crate) struct Store<F> {
    /// 0 = newest-wins; 1..=3 = the FIFO's bound against a burst.
    capacity: usize,
    frames: VecDeque<Held<F>>,
    /// The source-cadence loop; `None` under latency.
    cadence: Option<CadenceClock>,
    /// The source's frame interval, the loop's cushion ceiling ([`super::frame_interval_ns`]).
    frame_interval_ns: i64,
}

impl<F> Store<F> {
    /// `snapping()` tuning, because every present lands on a vsync grid whose snap-up carries
    /// about half a refresh of implicit slack.
    pub(crate) fn new(priority: PresentPriority, frame_interval_ns: i64) -> Store<F> {
        let (capacity, cadence) = match priority {
            PresentPriority::Latency => (0, None),
            PresentPriority::Smooth { buffer } => {
                (buffer, Some(CadenceClock::new(CadenceTuning::snapping())))
            }
        };
        Store {
            capacity,
            frames: VecDeque::new(),
            cadence,
            frame_interval_ns,
        }
    }

    pub(crate) fn newest_wins(&self) -> bool {
        self.capacity == 0
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Frames queued in the smoothing FIFO; 0 under newest-wins, whose one frame is a candidate.
    pub(crate) fn fifo_len(&self) -> usize {
        if self.newest_wins() {
            0
        } else {
            self.frames.len()
        }
    }

    /// Admit one decoded frame. Newest-wins evicts every older frame, the FIFO its oldest past
    /// capacity; each eviction goes to `evict` and counts in `skipped`. Returns the evictions.
    ///
    /// `pts_us` is the host's own stamp as the codec echoes it; `ready_mono_ns` is when the frame
    /// became presentable. Under smooth they fold into the due time, fed and read in
    /// `CLOCK_MONOTONIC` only: the loop's offset estimator absorbs the constant between the
    /// host's clock and ours, and smooths the offset, never the timestamps.
    pub(crate) fn push(
        &mut self,
        frame: F,
        pts_us: u64,
        ready_mono_ns: i64,
        c: &mut Counters,
        mut evict: impl FnMut(F),
    ) -> u64 {
        let interval_ns = self.frame_interval_ns;
        let due_ns = self
            .cadence
            .as_mut()
            .map(|cc| cc.due_ns(pts_us.saturating_mul(1_000), ready_mono_ns, interval_ns));
        let mut evicted = 0u64;
        if self.capacity == 0 {
            while let Some(old) = self.frames.pop_front() {
                evict(old.frame);
                evicted += 1;
            }
        }
        self.frames.push_back(Held { frame, due_ns });
        if self.capacity > 0 {
            while self.frames.len() > self.capacity {
                if let Some(old) = self.frames.pop_front() {
                    evict(old.frame);
                    evicted += 1;
                }
            }
        }
        c.skipped += evicted;
        evicted
    }

    /// Whether the head may leave by `reach_ns`: due by then, or carrying no due time. The
    /// store holds what is not due yet; the sink sets the reach.
    pub(crate) fn head_is_releasable(&self, reach_ns: i64) -> bool {
        self.frames
            .front()
            .is_some_and(|f| f.due_ns.is_none_or(|due| due <= reach_ns))
    }

    /// The frame a present takes: the newest under newest-wins, the oldest from the FIFO.
    pub(crate) fn take(&mut self) -> Option<Held<F>> {
        if self.newest_wins() {
            self.frames.pop_back()
        } else {
            self.frames.pop_front()
        }
    }

    /// Put back the frame [`Store::take`] returned when the budget was closed. A fresher push
    /// still evicts it under newest-wins.
    pub(crate) fn park(&mut self, held: Held<F>) {
        if self.newest_wins() {
            self.frames.push_back(held);
        } else {
            self.frames.push_front(held);
        }
    }

    /// Every held frame, oldest first, for the teardown.
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = F> + '_ {
        self.frames.drain(..).map(|h| h.frame)
    }

    /// Re-anchor the cadence loop on the next frame. The decode loop calls it when the re-anchor
    /// gate arms: past a loss the frames come through a decoder that just recovered, so the
    /// source→presentable delay the loop measured no longer holds. No-op under latency.
    pub(crate) fn reset_cadence(&mut self) {
        if let Some(c) = self.cadence.as_mut() {
            c.reset();
        }
    }

    /// The cadence loop's health for the 1 Hz line, or `None` when there is no loop.
    pub(crate) fn cadence_health(&self) -> Option<CadenceHealth> {
        self.cadence.as_ref().map(CadenceClock::health)
    }
}

#[cfg(test)]
mod tests {
    use super::super::budget::TimelineBudget;
    use super::super::frame_interval_ns;
    use super::*;

    const SOURCE_HZ: u32 = 120;
    const P: i64 = 8_333_333; // one 120 Hz source interval, ns
    /// A host stamping realtime against a monotonic present clock: two eras, which is why the
    /// loop estimates the offset rather than being told it.
    const PTS0_NS: i64 = 1_786_000_000_000_000_000;
    const MONO0: i64 = 987_000_000_000;

    /// `k`'s pts as the codec echoes it (µs, truncated as the feed queues it) and the monotonic
    /// instant it became presentable, with `jitter_ns` of transport noise on the arrival only.
    fn frame(k: i64, jitter_ns: i64) -> (u64, i64) {
        (((PTS0_NS + k * P) / 1000) as u64, MONO0 + k * P + jitter_ns)
    }

    fn store(priority: PresentPriority) -> Store<i64> {
        Store::new(priority, frame_interval_ns(SOURCE_HZ))
    }

    #[test]
    fn the_latency_store_never_consults_the_cadence_clock() {
        let mut s = store(PresentPriority::Latency);
        let mut c = Counters::default();
        for k in 0..600 {
            let (pts_us, ready) = frame(k, (k % 11) * 400_000);
            s.push(k, pts_us, ready, &mut c, drop);
            assert_eq!(
                s.frames.back().and_then(|h| h.due_ns),
                None,
                "latency must produce no due time at all"
            );
        }
        assert!(
            s.cadence_health().is_none(),
            "latency holds no loop to have health"
        );
    }

    #[test]
    fn the_smooth_store_puts_every_decoded_frame_through_the_loop() {
        let mut s = store(PresentPriority::Smooth { buffer: 2 });
        let mut c = Counters::default();
        for k in 0..600 {
            let (pts_us, ready) = frame(k, (k % 11) * 400_000);
            s.push(k, pts_us, ready, &mut c, drop);
            assert!(s.frames.back().is_some_and(|h| h.due_ns.is_some()));
        }
        let h = s.cadence_health().expect("smooth holds a loop");
        assert_eq!(h.frames, 600);
        assert_eq!(
            h.reanchors, 1,
            "only the cold start anchors on a clean trace"
        );
        s.reset_cadence();
        let (pts_us, ready) = frame(600, 0);
        s.push(600, pts_us, ready, &mut c, drop);
        assert_eq!(s.cadence_health().map(|h| h.reanchors), Some(2));
    }

    #[test]
    fn newest_wins_evicts_every_older_frame_and_counts_it() {
        let mut s = store(PresentPriority::Latency);
        let mut c = Counters::default();
        let mut evicted = Vec::new();
        for k in 0..3 {
            let (pts_us, ready) = frame(k, 0);
            let n = s.push(k, pts_us, ready, &mut c, |f| evicted.push(f));
            assert_eq!(n, u64::from(k > 0));
        }
        assert_eq!(evicted, [0, 1]);
        assert_eq!(c.skipped, 2);
        assert_eq!(s.fifo_len(), 0, "the candidate is not a queue");
        let newest = s.take().map(|h| h.frame);
        assert_eq!(newest, Some(2));
        assert!(s.is_empty());
        // A parked candidate is still the one a fresher frame evicts.
        let (pts_us, ready) = frame(3, 0);
        s.push(3, pts_us, ready, &mut c, drop);
        let held = s.take().expect("held");
        s.park(held);
        let (pts_us, ready) = frame(4, 0);
        assert_eq!(s.push(4, pts_us, ready, &mut c, drop), 1);
        assert_eq!(s.take().map(|h| h.frame), Some(4));
    }

    #[test]
    fn the_fifo_is_bounded_and_drains_oldest_first() {
        let mut s = store(PresentPriority::Smooth { buffer: 2 });
        let mut c = Counters::default();
        let mut evicted = Vec::new();
        for k in 0..4 {
            let (pts_us, ready) = frame(k, 0);
            s.push(k, pts_us, ready, &mut c, |f| evicted.push(f));
        }
        assert_eq!(evicted, [0, 1]);
        assert_eq!((c.skipped, s.fifo_len()), (2, 2));
        let head = s.take().expect("head");
        assert_eq!(head.frame, 2);
        s.park(head);
        assert_eq!(s.drain().collect::<Vec<_>>(), [2, 3]);
        assert!(s.is_empty());
    }

    #[test]
    fn the_store_holds_a_frame_that_is_not_due_yet_and_releases_it_within_reach_of_its_slot() {
        let mut s = store(PresentPriority::Smooth { buffer: 2 });
        let b = TimelineBudget::new(None);
        let due = MONO0 + 4 * P;
        s.frames.push_back(Held {
            frame: 0,
            due_ns: Some(due),
        });
        assert!(
            !s.head_is_releasable(b.reach_ns(MONO0, P)),
            "four refreshes early"
        );
        assert!(
            !s.head_is_releasable(b.reach_ns(due - P - b.margin_ns() - 1, P)),
            "one nanosecond outside the submit reach of its own slot"
        );
        assert!(
            s.head_is_releasable(b.reach_ns(due - P - b.margin_ns(), P)),
            "exactly one panel period of reach, the earliest that still buys nothing to wait"
        );
        assert!(
            s.head_is_releasable(b.reach_ns(due + 10 * P, P)),
            "late frames go at once"
        );
    }

    #[test]
    fn a_frame_carrying_no_due_time_is_always_releasable() {
        // The predicate drains a frame it cannot answer for rather than wedge behind it.
        let mut s = store(PresentPriority::Smooth { buffer: 2 });
        s.frames.push_back(Held {
            frame: 0,
            due_ns: None,
        });
        assert!(s.head_is_releasable(MONO0));
        assert!(!store(PresentPriority::Latency).head_is_releasable(MONO0));
    }
}
