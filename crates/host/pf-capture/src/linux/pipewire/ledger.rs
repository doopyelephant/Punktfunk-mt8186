//! A pool buffer's life on this side, from `add_buffer` to `remove_buffer`: one state per
//! buffer, and one [`step`] over what the loop thread and the encoder do to it.

use super::hold::SHALLOW_POOL;

/// Where one pool buffer is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum BufferState {
    /// With the producer.
    #[default]
    Free,
    /// In this `.process`, neither held nor handed back yet.
    Dequeued,
    /// Withheld from the producer under the hold of `generation`.
    Held { generation: u64 },
    /// Its hold dropped, the stream's `nth` drop; withheld until the loop requeues it.
    Released { generation: u64, nth: u64 },
    /// Freed under us by `remove_buffer`.
    Purged,
}

/// What happens to a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Event {
    /// This `.process` takes the buffer, to publish it or to hand it back.
    Dequeued,
    /// A hold of this generation goes out with the buffer's frame.
    Published(u64),
    /// The hold of `generation` dropped, on any thread: the stream's `nth` drop.
    HoldDropped { generation: u64, nth: u64 },
    /// The loop requeues what was released.
    Drained,
    /// This `.process` is done with the buffer.
    Done,
    /// `remove_buffer`.
    Removed,
}

/// What the loop thread does with the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Nothing,
    /// Hand it back to the producer.
    Requeue,
    /// The producer sent a held buffer again (PipeWire < 1.6). The stale hold is gone, so its
    /// release cannot requeue the buffer this `.process` publishes; one IDR covers the frame
    /// encoded from the earlier read.
    Resent,
}

/// One buffer's next state. A hold releases only under its own generation, so a stale hold
/// never requeues a re-held buffer or the next pool's tenant of its address.
pub(super) fn step(state: BufferState, event: Event) -> (BufferState, Action) {
    use BufferState::*;
    match (state, event) {
        (Purged, _) | (_, Event::Removed) => (Purged, Action::Nothing),
        (Held { .. } | Released { .. }, Event::Dequeued) => (Dequeued, Action::Resent),
        (_, Event::Dequeued) => (Dequeued, Action::Nothing),
        (_, Event::Published(generation)) => (Held { generation }, Action::Nothing),
        (
            Held { generation },
            Event::HoldDropped {
                generation: dropped,
                nth,
            },
        ) if dropped == generation => (Released { generation, nth }, Action::Nothing),
        (Released { .. }, Event::Drained) => (Free, Action::Requeue),
        (Free | Dequeued, Event::Done) => (Free, Action::Requeue),
        (state, _) => (state, Action::Nothing),
    }
}

/// One pool buffer's entry.
#[derive(Default)]
struct Entry {
    state: BufferState,
}

/// Every buffer of the pool, in the order the producer announced them.
#[derive(Default)]
pub(super) struct Book {
    bufs: Vec<(usize, Entry)>,
    /// Last issued hold generation, monotonic per stream.
    last_gen: u64,
    /// Holds dropped so far: the drain requeues in drop order.
    drops: u64,
}

impl Book {
    /// Step `buf` through `event`. A buffer the book does not list is `Free` to the loop's own
    /// events on a buffer in its hands, and `Purged` to the rest.
    fn apply(&mut self, buf: usize, event: Event) -> Action {
        let at = self.bufs.iter().position(|(b, _)| *b == buf);
        let state = match at {
            Some(i) => self.bufs[i].1.state,
            None if matches!(event, Event::Dequeued | Event::Published(_) | Event::Done) => {
                BufferState::Free
            }
            None => BufferState::Purged,
        };
        let (next, action) = step(state, event);
        match (at, next) {
            (Some(i), BufferState::Purged) => {
                self.bufs.remove(i);
            }
            (Some(i), next) => self.bufs[i].1.state = next,
            (None, BufferState::Purged) => {}
            (None, state) => self.bufs.push((buf, Entry { state })),
        }
        action
    }

    /// `add_buffer`.
    pub(super) fn add(&mut self, buf: usize) {
        self.bufs.retain(|(b, _)| *b != buf);
        self.bufs.push((buf, Entry::default()));
    }

    /// `remove_buffer`: a later release of its hold is a no-op.
    pub(super) fn remove(&mut self, buf: usize) {
        self.apply(buf, Event::Removed);
    }

    /// `buf` arrives in `.process`. `true`: the producer re-sent it while held.
    pub(super) fn arrive(&mut self, buf: usize) -> bool {
        self.apply(buf, Event::Dequeued) == Action::Resent
    }

    /// Withhold `buf` under a new generation, unless the pool is too shallow to lend
    /// ([`SHALLOW_POOL`]) or every other buffer is out.
    pub(super) fn hold(&mut self, buf: usize, pool_live: u32) -> Option<u64> {
        let cap = if pool_live > SHALLOW_POOL {
            pool_live as usize
        } else {
            0
        };
        let out = self
            .bufs
            .iter()
            .filter(|(b, e)| *b != buf && withheld(e.state));
        if out.count() >= cap {
            return None;
        }
        self.last_gen += 1;
        self.apply(buf, Event::Published(self.last_gen));
        Some(self.last_gen)
    }

    /// The hold of `generation` on `buf` dropped.
    pub(super) fn drop_hold(&mut self, buf: usize, generation: u64) {
        self.drops += 1;
        let nth = self.drops;
        self.apply(buf, Event::HoldDropped { generation, nth });
    }

    /// Every released buffer, in drop order, each now `Free`: the caller requeues them.
    pub(super) fn drain(&mut self) -> Vec<usize> {
        let mut released: Vec<(u64, usize)> = self
            .bufs
            .iter()
            .filter_map(|(b, e)| match e.state {
                BufferState::Released { nth, .. } => Some((nth, *b)),
                _ => None,
            })
            .collect();
        released.sort_unstable();
        for &(_, buf) in &released {
            self.apply(buf, Event::Drained);
        }
        released.into_iter().map(|(_, buf)| buf).collect()
    }

    /// `.process` is done with `buf`. `true`: hand it back now; `false`: a hold owns it.
    pub(super) fn done(&mut self, buf: usize) -> bool {
        self.apply(buf, Event::Done) == Action::Requeue
    }
}

/// Withheld from the producer: a hold is out, or dropped and not yet requeued.
fn withheld(state: BufferState) -> bool {
    matches!(
        state,
        BufferState::Held { .. } | BufferState::Released { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::{Book, SHALLOW_POOL};

    #[derive(Clone, Copy, Debug)]
    enum Op {
        /// The census: how many buffers the pool has.
        Pool(u32),
        Add(usize),
        Remove(usize),
        Arrive(usize),
        Hold(usize),
        Drop(usize, u64),
        Drain,
        Done(usize),
    }

    #[derive(Debug, PartialEq)]
    enum Saw {
        /// The op answers nothing.
        Ok,
        Resent,
        Held(u64),
        Refused,
        Requeued(&'static [usize]),
        /// `.process` hands the buffer back.
        Back,
        /// A hold owns the requeue.
        Kept,
    }

    const A: usize = 0xa;
    const B: usize = 0xb;
    const C: usize = 0xc;
    const D: usize = 0xd;
    const E: usize = 0xe;

    struct Row {
        /// The fix this row pins.
        fix: &'static str,
        /// The tests it is taken from.
        from: &'static str,
        steps: &'static [(Op, Saw)],
    }

    use Op::*;
    use Saw::{Back, Held, Kept, Refused, Requeued, Resent};

    const KWIN: u32 = crate::KWIN_POOL_MAX as u32;

    const ROWS: &[Row] = &[
        Row {
            fix: "12e428be0 re-hold a re-sent buffer instead of rebuilding",
            from: "hold_book_rehold_spends_no_second_slot, \
                   a_resent_buffer_is_reheld_under_a_new_generation",
            steps: &[
                (Pool(SHALLOW_POOL + 1), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Done(A), Kept),
                (Arrive(B), Saw::Ok),
                (Hold(B), Held(2)),
                (Done(B), Kept),
                (Arrive(C), Saw::Ok),
                (Hold(C), Held(3)),
                (Done(C), Kept),
                (Arrive(D), Saw::Ok),
                (Hold(D), Refused),
                (Done(D), Back),
                (Arrive(A), Resent),
                (Hold(A), Held(4)),
                (Done(A), Kept),
                (Drop(A, 1), Saw::Ok),
                (Drain, Requeued(&[])),
                (Drop(A, 4), Saw::Ok),
                (Drain, Requeued(&[A])),
            ],
        },
        Row {
            fix: "20c346a90 a hold shortage drops the arrival, never copies",
            from: "hold_book_lends_the_whole_pool_or_nothing, a_kwin_pool_lends_all_four, \
                   an_arrival_spends_only_a_hold_that_came_back",
            steps: &[
                (Pool(SHALLOW_POOL), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Refused),
                (Done(A), Back),
                (Pool(KWIN), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Hold(B), Held(2)),
                (Hold(C), Held(3)),
                (Hold(D), Held(4)),
                (Arrive(E), Saw::Ok),
                (Hold(E), Refused),
                (Done(E), Back),
                (Drop(B, 2), Saw::Ok),
                (Drain, Requeued(&[B])),
                (Arrive(E), Saw::Ok),
                (Hold(E), Held(5)),
                (Drop(B, 2), Saw::Ok),
                (Drain, Requeued(&[])),
            ],
        },
        Row {
            fix: "ee709266d requeue a dropped hold before the arrival gives up",
            from: "an_arrival_drains_a_release_the_loop_has_not_serviced",
            steps: &[
                (Pool(KWIN), Saw::Ok),
                (Hold(A), Held(1)),
                (Hold(B), Held(2)),
                (Hold(C), Held(3)),
                (Hold(D), Held(4)),
                (Drop(A, 1), Saw::Ok),
                (Arrive(E), Saw::Ok),
                (Hold(E), Refused),
                (Drain, Requeued(&[A])),
                (Hold(E), Held(5)),
                (Done(E), Kept),
                (Drain, Requeued(&[])),
            ],
        },
        Row {
            fix: "2b771fe20 purge held buffers before handing them back",
            from: "a_burst_hands_its_stale_buffers_back_once",
            steps: &[
                (Pool(KWIN), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Done(A), Kept),
                (Arrive(A), Resent),
                (Done(A), Back),
                (Arrive(B), Saw::Ok),
                (Done(B), Back),
                (Arrive(C), Saw::Ok),
                (Drain, Requeued(&[])),
                (Hold(C), Held(2)),
                (Done(C), Kept),
                (Drop(A, 1), Saw::Ok),
                (Drain, Requeued(&[])),
                (Drop(C, 2), Saw::Ok),
                (Drain, Requeued(&[C])),
            ],
        },
        Row {
            fix: "a re-sent buffer is re-held under the new frame",
            from: "a_resent_buffer_is_reheld_under_the_new_frame",
            steps: &[
                (Pool(KWIN), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Done(A), Kept),
                (Arrive(A), Resent),
                (Drain, Requeued(&[])),
                (Hold(A), Held(2)),
                (Done(A), Kept),
                (Drop(A, 1), Saw::Ok),
                (Drain, Requeued(&[])),
                (Drop(A, 2), Saw::Ok),
                (Drain, Requeued(&[A])),
            ],
        },
        Row {
            fix: "4f2bb5bbe purge a re-sent buffer before draining releases",
            from: "a_resent_buffer_is_not_handed_back_by_its_stale_release",
            steps: &[
                (Pool(KWIN), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Done(A), Kept),
                (Drop(A, 1), Saw::Ok),
                (Arrive(A), Resent),
                (Drain, Requeued(&[])),
                (Hold(A), Held(2)),
                (Done(A), Kept),
                (Drop(A, 2), Saw::Ok),
                (Drain, Requeued(&[A])),
            ],
        },
        Row {
            fix: "one hold, one requeue",
            from: "a_held_buffer_rejoins_once_after_its_hold_drops, \
                   hold_book_releases_exactly_once",
            steps: &[
                (Pool(KWIN), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Done(A), Kept),
                (Drain, Requeued(&[])),
                (Drop(A, 1), Saw::Ok),
                (Drain, Requeued(&[A])),
                (Drop(A, 1), Saw::Ok),
                (Drain, Requeued(&[])),
            ],
        },
        Row {
            fix: "a renegotiated pool takes no release from the old one",
            from: "a_renegotiated_pool_takes_no_release_from_the_old_one, \
                   hold_book_generation_outlives_an_address_reuse",
            steps: &[
                (Pool(KWIN), Saw::Ok),
                (Add(A), Saw::Ok),
                (Add(B), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Done(A), Kept),
                (Arrive(B), Saw::Ok),
                (Hold(B), Held(2)),
                (Done(B), Kept),
                (Remove(A), Saw::Ok),
                (Remove(B), Saw::Ok),
                (Drop(A, 1), Saw::Ok),
                (Drain, Requeued(&[])),
                (Add(A), Saw::Ok),
                (Add(B), Saw::Ok),
                (Arrive(B), Saw::Ok),
                (Hold(B), Held(3)),
                (Done(B), Kept),
                (Drop(B, 2), Saw::Ok),
                (Drain, Requeued(&[])),
                (Drop(B, 3), Saw::Ok),
                (Drain, Requeued(&[B])),
            ],
        },
    ];

    fn run(book: &mut Book, pool: &mut u32, op: Op) -> Saw {
        match op {
            Pool(n) => *pool = n,
            Add(buf) => book.add(buf),
            Remove(buf) => book.remove(buf),
            Arrive(buf) if book.arrive(buf) => return Resent,
            Arrive(_) => {}
            Hold(buf) => return book.hold(buf, *pool).map_or(Refused, Held),
            Drop(buf, generation) => book.drop_hold(buf, generation),
            Drain => return Requeued(book.drain().leak()),
            Done(buf) if book.done(buf) => return Back,
            Done(_) => return Kept,
        }
        Saw::Ok
    }

    /// Each row replays what a fix pinned, one buffer event at a time.
    #[test]
    fn a_pool_buffer_lives_as_each_fix_pinned_it() {
        for row in ROWS {
            let (mut book, mut pool) = (Book::default(), 0);
            for (i, (op, want)) in row.steps.iter().enumerate() {
                let saw = run(&mut book, &mut pool, *op);
                assert_eq!(&saw, want, "{} ({}), step {i}: {op:?}", row.fix, row.from);
            }
        }
    }

    /// Two holds dropped before the loop runs its wake rejoin in the order they dropped.
    #[test]
    fn released_buffers_rejoin_in_drop_order() {
        let mut book = Book::default();
        for buf in [A, B, C] {
            book.hold(buf, 4).unwrap();
        }
        book.drop_hold(C, 3);
        book.drop_hold(A, 1);
        assert_eq!(book.drain(), [C, A]);
    }
}
