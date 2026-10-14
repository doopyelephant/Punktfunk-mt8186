//! The smoothness drain's slot pick: one present per panel latch slot, aimed a margin
//! ahead of now. Pure; `StreamState::pick` hands it the latch clock and the frame store.
//! The margin starts at 0, since a fixed lead is display tax, and widens one step per
//! window whose measured latch misses demand it.
//!
//! Pinned by the table test below.

/// Slot-pick margin: 0.5 ms step, 2.5 ms cap.
pub(crate) const MARGIN_STEP_NS: u64 = 500_000;
pub(crate) const MARGIN_MAX_NS: u64 = 2_500_000;

#[derive(Default)]
pub(crate) struct SlotPick {
    /// Submit lead the next reachable slot is reckoned from.
    margin_ns: u64,
    /// The latch slot the last vended frame was aimed at.
    last_slot_ns: u64,
}

impl SlotPick {
    pub(crate) fn margin_ns(&self) -> u64 {
        self.margin_ns
    }

    /// The frame for the first latch still reachable from `now_ns`. `slot_after` is the
    /// latch clock's first slot after a time; `take` vends the store's frame due before a
    /// slot. A frame due before that slot cannot be shown sooner by waiting; one due after
    /// it would land a slot early. Two frames due before one slot would go back to back,
    /// and MAILBOX would show one for nothing while the next slot went empty.
    pub(crate) fn pick<T>(
        &mut self,
        now_ns: u64,
        slot_after: impl FnOnce(u64) -> u64,
        take: impl FnOnce(u64) -> Option<T>,
    ) -> Option<T> {
        let slot = slot_after(now_ns.saturating_add(self.margin_ns));
        if slot == self.last_slot_ns {
            return None;
        }
        let taken = take(slot);
        if taken.is_some() {
            self.last_slot_ns = slot;
        }
        taken
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

    /// A new panel: the margin was sized by the old one's misses.
    pub(crate) fn reset_margin(&mut self) {
        self.margin_ns = 0;
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

    /// (case, widen steps, frames' due times in ms, picks as (now ms, vended due ms))
    type Row = (
        &'static str,
        u32,
        &'static [u64],
        &'static [(u64, Option<u64>)],
    );

    #[test]
    fn one_present_per_latch_slot() {
        let table: &[Row] = &[
            (
                "b5917027b: a second frame due before the same slot waits for the next",
                0,
                &[1, 2],
                &[(0, Some(1)), (1, None), (9, None), (10, Some(2))],
            ),
            (
                "an empty take leaves the slot open",
                0,
                &[15],
                &[(0, None), (5, None), (10, Some(15))],
            ),
            (
                "the margin aims at the slot after the submit lead",
                5,
                &[12],
                &[(8, Some(12))],
            ),
            (
                "no margin: that frame waits a slot",
                0,
                &[12],
                &[(8, None), (10, Some(12))],
            ),
        ];
        for &(case, steps, due, picks) in table {
            let mut s = SlotPick::default();
            for _ in 0..steps {
                s.widen(3);
            }
            let mut store: VecDeque<u64> = due.iter().map(|d| d * MS).collect();
            for &(now, want) in picks {
                let got = s.pick(now * MS, slot_after, |slot| {
                    store
                        .front()
                        .is_some_and(|&d| d < slot)
                        .then(|| store.pop_front())?
                });
                assert_eq!(got, want.map(|d| d * MS), "{case}: pick at {now} ms");
            }
        }
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
            (3, true, 2_000),
            (3, true, 2_500),
            (40, false, 2_500),
        ];
        for &(misses, widened, margin_us) in table {
            assert_eq!(s.widen(misses), widened, "{misses} misses");
            assert_eq!(s.margin_ns(), margin_us * 1000, "{misses} misses");
        }
        s.reset_margin();
        assert_eq!(s.margin_ns(), 0);
    }
}
