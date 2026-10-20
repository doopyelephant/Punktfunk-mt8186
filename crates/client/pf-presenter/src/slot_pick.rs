//! The paced drain's slot pick: a frame goes out at its due time, never before, aimed at
//! the first latch its compositor can still make — one frame per slot. Pure;
//! `StreamState::pick` hands it the latch clock and the frame store.
//!
//! The lead is what the compositor needs between a present and the latch it lands on.
//! It starts at a whole period plus a margin and learns: a frame that lands past its aim
//! lengthens it, a window of frames landing before their aim shortens it. The aim is
//! bookkeeping for the ledger and the learner; the present instant stays the due time.
//!
//! Pinned by the table test below.

/// Slot-pick margin: 0.5 ms step, 8 ms cap (KWin on NVIDIA wants a period and a half).
pub(crate) const MARGIN_STEP_NS: u64 = 500_000;
pub(crate) const MARGIN_MAX_NS: u64 = 8_000_000;

/// Learned lead: a window of early landings shortens it by this much; a slip gives four
/// steps back. The lead never drops under [`LEAD_MIN_NS`].
const SHRINK_STEP_NS: u64 = 250_000;
/// 2 ms: our own submit plus a fast compositor's latch.
const LEAD_MIN_NS: u64 = 2_000_000;

#[derive(Default)]
pub(crate) struct SlotPick {
    /// Lead past the whole period, widened by missed slots.
    margin_ns: u64,
    /// The latch slot the last vended frame was aimed at.
    last_slot_ns: u64,
    /// How much under a whole period the lead runs, earned by early landings.
    shrink_ns: u64,
}

impl SlotPick {
    pub(crate) fn margin_ns(&self) -> u64 {
        self.margin_ns
    }

    /// The latch the last vended frame was aimed at; 0 before the first.
    pub(crate) fn last_target_ns(&self) -> u64 {
        self.last_slot_ns
    }

    /// What the compositor needs between a present and the latch it lands on.
    pub(crate) fn lead_ns(&self, period_ns: u64) -> u64 {
        period_ns - self.shrink_ns.min(period_ns.saturating_sub(LEAD_MIN_NS)) + self.margin_ns
    }

    /// The latch a frame presented at `due_ns` lands on: the first at least a lead after
    /// it, or the slot after the last vended one — two frames never share a slot.
    fn target_ns(&self, due_ns: u64, period_ns: u64, slot_after: impl FnOnce(u64) -> u64) -> u64 {
        let t = slot_after(
            due_ns
                .saturating_add(self.lead_ns(period_ns))
                .saturating_sub(1),
        );
        if t <= self.last_slot_ns {
            self.last_slot_ns + period_ns
        } else {
            t
        }
    }

    /// Vend the front frame (due at `front_due_ns`) at its due time. `slot_after` is the
    /// latch clock's first slot after a time; `take` pops the store.
    pub(crate) fn pick<T>(
        &mut self,
        now_ns: u64,
        period_ns: u64,
        front_due_ns: Option<u64>,
        slot_after: impl FnOnce(u64) -> u64,
        take: impl FnOnce() -> Option<T>,
    ) -> Option<T> {
        let due = front_due_ns?;
        if now_ns < due {
            return None;
        }
        let target = self.target_ns(due, period_ns, slot_after);
        let taken = take();
        if taken.is_some() {
            self.last_slot_ns = target;
        }
        taken
    }

    /// A vended frame went back to the store (the glass gate parked it, the presenter was
    /// busy): its slot is free again. `mark` is [`Self::last_target_ns`] from before the
    /// pick, so the retry aims at the same latch instead of one period later.
    pub(crate) fn rewind(&mut self, mark: u64) {
        self.last_slot_ns = mark;
    }

    /// One step wider after a window with more than two latch misses; one-way per stream
    /// until [`Self::reset_margin`]. `true` when it widened.
    pub(crate) fn widen(&mut self, misses: u32) -> bool {
        if misses <= 2 || self.margin_ns >= MARGIN_MAX_NS {
            return false;
        }
        self.margin_ns = (self.margin_ns + MARGIN_STEP_NS).min(MARGIN_MAX_NS);
        true
    }

    /// A window closed: `misses` frames landed past their aimed latch, `early` before it.
    /// Whichever side dominates moves the lead its way: four steps longer, one step
    /// shorter. A window split between the two says the aim sits on a latch and the
    /// stamps straddle it; nothing moves. The shrink stops where the lead meets its
    /// floor, so a slip lifts the lead at once instead of paying off a wound-up count.
    pub(crate) fn note_window(&mut self, misses: u32, early: u32, period_ns: u64) {
        if misses > early.saturating_mul(2) {
            if self.shrink_ns > 0 {
                self.shrink_ns = self.shrink_ns.saturating_sub(4 * SHRINK_STEP_NS);
            } else {
                self.margin_ns = (self.margin_ns + MARGIN_STEP_NS).min(MARGIN_MAX_NS);
            }
        } else if early > misses.saturating_mul(2) {
            if self.margin_ns > 0 {
                self.margin_ns = self.margin_ns.saturating_sub(MARGIN_STEP_NS);
            } else {
                let floor = period_ns.saturating_sub(LEAD_MIN_NS);
                self.shrink_ns = (self.shrink_ns + SHRINK_STEP_NS).min(floor);
            }
        }
    }

    /// A new panel: the lead was sized by the old one's compositor.
    pub(crate) fn reset_margin(&mut self) {
        self.margin_ns = 0;
        self.shrink_ns = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const MS: u64 = 1_000_000;
    /// A 100 Hz grid anchored at 0, as `LatchClock::next_slot_after` reckons it.
    const PERIOD: u64 = 10 * MS;

    fn slot_after(t: u64) -> u64 {
        (t / PERIOD + 1) * PERIOD
    }

    /// A frame is never presented before its due time, whatever the grid says.
    #[test]
    fn a_frame_goes_out_at_its_due_time_never_before() {
        let mut s = SlotPick::default();
        let mut store: VecDeque<u64> = [12 * MS, 29 * MS].into();
        // (now ms, vended due ms)
        for (now, want) in [
            (0, None),
            (11, None),
            (12, Some(12)),
            (20, None),
            (29, Some(29)),
        ] {
            let front = store.front().copied();
            let got = s.pick(now * MS, PERIOD, front, slot_after, || store.pop_front());
            assert_eq!(got, want.map(|d: u64| d * MS), "pick at {now} ms");
        }
    }

    /// The aim is the first latch at least a lead after the due time, and two frames
    /// never share one: the second aims a slot later.
    #[test]
    fn the_aim_is_the_first_latch_a_lead_after_due_one_per_slot() {
        let mut s = SlotPick::default();
        assert_eq!(s.lead_ns(PERIOD), PERIOD, "a whole period to start with");
        let mut store: VecDeque<u64> = [12 * MS, 13 * MS, 41 * MS].into();
        s.pick(12 * MS, PERIOD, store.front().copied(), slot_after, || {
            store.pop_front()
        });
        assert_eq!(
            s.last_target_ns(),
            30 * MS,
            "due 12 + lead 10 = 22 → the latch at 30"
        );
        s.pick(13 * MS, PERIOD, store.front().copied(), slot_after, || {
            store.pop_front()
        });
        assert_eq!(
            s.last_target_ns(),
            40 * MS,
            "the slot after the one just taken"
        );
        s.pick(41 * MS, PERIOD, store.front().copied(), slot_after, || {
            store.pop_front()
        });
        assert_eq!(
            s.last_target_ns(),
            60 * MS,
            "due 41 + 10 = 51 → 60; exactly on a latch counts"
        );
    }

    /// Early landings shorten the lead a step per window down to half a period; one
    /// slip gives four steps back; missed slots widen the margin past the period.
    #[test]
    fn the_lead_learns_from_where_frames_land() {
        let mut s = SlotPick::default();
        for _ in 0..8 {
            s.note_window(0, 3, PERIOD);
        }
        assert_eq!(s.lead_ns(PERIOD), 8 * MS);
        s.note_window(1, 0, PERIOD);
        assert_eq!(s.lead_ns(PERIOD), 9 * MS, "a slip costs 1 ms");
        for _ in 0..40 {
            s.note_window(0, 1, PERIOD);
        }
        assert_eq!(s.lead_ns(PERIOD), LEAD_MIN_NS, "never under the floor");
        s.note_window(0, 0, PERIOD);
        assert_eq!(
            s.lead_ns(PERIOD),
            LEAD_MIN_NS,
            "a window with no evidence changes nothing"
        );
        s.note_window(1, 0, PERIOD);
        assert_eq!(
            s.lead_ns(PERIOD),
            LEAD_MIN_NS + MS,
            "the shrink stops at the floor: a slip lifts the lead at once"
        );
        s.reset_margin();
        assert_eq!(s.lead_ns(PERIOD), PERIOD);
        for _ in 0..20 {
            s.widen(3);
        }
        assert_eq!(
            s.lead_ns(PERIOD),
            PERIOD + MARGIN_MAX_NS,
            "misses widen up to the cap"
        );
    }

    #[test]
    fn the_margin_widens_on_misses_up_to_its_cap() {
        let mut s = SlotPick::default();
        // (misses, widened, margin after) in µs
        let table: &[(u32, bool, u64)] = &[
            (0, false, 0),
            (2, false, 0),
            (3, true, 500),
            (9, true, 1_000),
            (3, true, 1_500),
            (40, true, 2_000),
        ];
        for &(misses, widened, margin_us) in table {
            assert_eq!(s.widen(misses), widened, "{misses} misses");
            assert_eq!(s.margin_ns(), margin_us * 1000, "{misses} misses");
        }
        for _ in 0..20 {
            s.widen(3);
        }
        assert_eq!(s.margin_ns(), MARGIN_MAX_NS);
        assert!(!s.widen(3), "capped");
        s.reset_margin();
        assert_eq!(s.margin_ns(), 0);
    }

    /// A frame the gate parks is picked again on the next pass; given its mark back, the
    /// retry aims at the same latch, not one period later per retry.
    #[test]
    fn a_parked_frame_keeps_its_aim_across_retries() {
        let mut s = SlotPick::default();
        let mut store: VecDeque<u64> = [12 * MS].into();
        for retry in 0..3u64 {
            let mark = s.last_target_ns();
            let now = 12 * MS + retry * 200_000;
            let got = s.pick(now, PERIOD, store.front().copied(), slot_after, || {
                store.pop_front()
            });
            assert_eq!(got, Some(12 * MS), "retry {retry} vends the frame");
            assert_eq!(
                s.last_target_ns(),
                30 * MS,
                "retry {retry} aims at the latch at 30"
            );
            store.push_front(12 * MS);
            s.rewind(mark);
        }
    }
}
