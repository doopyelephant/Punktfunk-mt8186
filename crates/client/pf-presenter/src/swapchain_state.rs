//! The swapchain's bookkeeping beside the presenter's Vulkan handles: who owns the window,
//! the submit in flight, the image held between acquire and submit, the stamp asks.
//!
//! Pure: no Vulkan. `vk::Presenter` performs what it answers. Ownership runs
//! `Swapchain → Lane { shown: false } → Lane { shown: true }`, and back to `Swapchain`
//! when a real frame needs the swapchain. A lane state means the swapchain and its
//! surface are torn down, so nothing may draw into them.
//!
//! Pinned by the table test below. The order inside a present (import, acquire, retire)
//! is `vk/present.rs`'s.

/// Who presents into the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    Swapchain,
    /// The native lane; swapchain and surface are torn down. `shown`: the last frame
    /// went through the lane, `pq` with a PQ description.
    Lane {
        shown: bool,
        pq: bool,
    },
}

/// What a frame arriving at `present` does before any GPU work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Draw,
    /// A redraw while the lane owns the window: it would paint over the lane's picture.
    Skip,
    /// A real frame takes the window back: surface and swapchain first, then draw.
    Resume,
}

/// The glass clocks that follow the swapchain's presents.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clocks {
    /// Present-wait, either generation.
    pub(crate) waiter: bool,
    /// The vblank waiter, whose stamps are estimates.
    pub(crate) vblank: bool,
}

pub(crate) struct SwapchainState {
    owner: Owner,
    /// Submit fence has work pending. Wait before recording; also what makes the single
    /// staging buffer safe to overwrite.
    pub(crate) submitted: bool,
    /// Swapchain image acquired and not yet submitted: the non-blocking probe's, or one an
    /// error left behind. Its `acquire_sem` signal is pending until a batch waits it.
    pub(crate) acquired: Option<u32>,
    /// The live swapchain was created for engine stamps and its result queue is sized.
    pub(crate) timing_armed: bool,
    /// The last present asked for a stamp.
    pub(crate) timing_asked: bool,
}

impl SwapchainState {
    pub(crate) fn new() -> SwapchainState {
        SwapchainState {
            owner: Owner::Swapchain,
            submitted: false,
            acquired: None,
            timing_armed: false,
            timing_asked: false,
        }
    }

    pub(crate) fn enter(&mut self, redraw: bool) -> Entry {
        match self.owner {
            Owner::Swapchain => Entry::Draw,
            Owner::Lane { .. } if redraw => Entry::Skip,
            Owner::Lane { .. } => {
                self.owner = Owner::Lane {
                    shown: false,
                    pq: false,
                };
                Entry::Resume
            }
        }
    }

    /// The swapchain and its surface are torn down while the lane owns the window.
    pub(crate) fn suspended(&self) -> bool {
        matches!(self.owner, Owner::Lane { .. })
    }

    /// The last frame shown went through the native lane, not the swapchain.
    pub(crate) fn native_last(&self) -> bool {
        matches!(self.owner, Owner::Lane { shown: true, .. })
    }

    /// The lane's last frame is PQ, shown through the compositor's colour management.
    pub(crate) fn native_pq(&self) -> bool {
        self.owner
            == Owner::Lane {
                shown: true,
                pq: true,
            }
    }

    // Only the Linux lane takes the window.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn suspend(&mut self) {
        self.owner = Owner::Lane {
            shown: false,
            pq: false,
        };
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn resume(&mut self) {
        self.owner = Owner::Swapchain;
    }

    /// The lane committed a frame. It only does while it owns the window.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn native_shown(&mut self, pq: bool) {
        debug_assert!(
            self.suspended(),
            "the lane shows only while it owns the window"
        );
        self.owner = Owner::Lane { shown: true, pq };
    }

    /// A submit waited the held image's `acquire_sem` and named the fence.
    pub(crate) fn on_submit(&mut self) {
        self.submitted = true;
        self.acquired = None;
    }

    /// Where the display stamp comes from: `feedback` (the lane), `timing` (the engine's
    /// own stamps), `wait` (present-wait), `est` (the vblank waiter), `none`.
    pub(crate) fn stamp_source(&self, c: Clocks) -> &'static str {
        if self.native_last() {
            "feedback"
        } else if self.timing_armed {
            "timing"
        } else if c.waiter {
            "wait"
        } else if c.vblank {
            "est"
        } else {
            "none"
        }
    }

    /// The display stamps are the vblank waiter's estimate.
    pub(crate) fn stamps_estimated(&self, c: Clocks) -> bool {
        !c.waiter && c.vblank && !self.native_last()
    }

    /// Present-wait, the vblank waiter or the lane's feedback drives the display stamp.
    pub(crate) fn stamps_active(&self, c: Clocks) -> bool {
        c.waiter || c.vblank || self.native_last()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One step of the window's ownership, as `vk::Presenter` drives it.
    #[derive(Clone, Copy, Debug)]
    enum Ev {
        Frame { redraw: bool },
        Suspend,
        Shown { pq: bool },
        Resume,
    }

    fn run(events: &[Ev]) -> (SwapchainState, Vec<Entry>) {
        let mut s = SwapchainState::new();
        let mut entries = Vec::new();
        for e in events {
            match *e {
                Ev::Frame { redraw } => entries.push(s.enter(redraw)),
                Ev::Suspend => s.suspend(),
                Ev::Shown { pq } => s.native_shown(pq),
                Ev::Resume => s.resume(),
            }
        }
        (s, entries)
    }

    const REAL: Ev = Ev::Frame { redraw: false };
    const REDRAW: Ev = Ev::Frame { redraw: true };

    /// (events, entries, suspended, native_last, native_pq)
    type OwnerRow = (&'static [Ev], &'static [Entry], bool, bool, bool);

    #[test]
    fn the_window_changes_hands_only_on_a_real_frame() {
        use Entry::*;
        let table: &[OwnerRow] = &[
            (&[REAL, REDRAW], &[Draw, Draw], false, false, false),
            // The lane took the window and showed a frame: a redraw paints nothing.
            (
                &[Ev::Suspend, Ev::Shown { pq: true }, REDRAW],
                &[Skip],
                true,
                true,
                true,
            ),
            // Suspended without a frame shown (the commit dropped): still the lane's.
            (&[Ev::Suspend, REDRAW], &[Skip], true, false, false),
            // A real frame takes the window back; the lane no longer shows.
            (
                &[Ev::Suspend, Ev::Shown { pq: true }, REAL],
                &[Resume],
                true,
                false,
                false,
            ),
            // A failed resume leaves the lane's state: the next real frame resumes again.
            (
                &[Ev::Suspend, Ev::Shown { pq: false }, REAL, REAL],
                &[Resume, Resume],
                true,
                false,
                false,
            ),
            (
                &[
                    Ev::Suspend,
                    Ev::Shown { pq: false },
                    REAL,
                    Ev::Resume,
                    REDRAW,
                ],
                &[Resume, Draw],
                false,
                false,
                false,
            ),
        ];
        for (events, entries, suspended, native_last, native_pq) in table {
            let (s, got) = run(events);
            assert_eq!(&got, entries, "{events:?}");
            assert_eq!(s.suspended(), *suspended, "suspended after {events:?}");
            assert_eq!(
                s.native_last(),
                *native_last,
                "native_last after {events:?}"
            );
            assert_eq!(s.native_pq(), *native_pq, "native_pq after {events:?}");
        }
    }

    /// (lane shown, armed, waiter, vblank) → (source, estimated, timing active)
    type GlassRow = ((bool, bool, bool, bool), (&'static str, bool, bool));

    #[test]
    fn the_display_stamp_source_ranks_lane_engine_wait_vblank() {
        let table: &[GlassRow] = &[
            ((true, true, true, true), ("feedback", false, true)),
            ((true, false, false, true), ("feedback", false, true)),
            ((false, true, true, false), ("timing", false, true)),
            ((false, false, true, true), ("wait", false, true)),
            ((false, false, false, true), ("est", true, true)),
            ((false, false, false, false), ("none", false, false)),
        ];
        for &((shown, armed, waiter, vblank), (source, estimated, active)) in table {
            let mut s = SwapchainState::new();
            if shown {
                s.suspend();
                s.native_shown(false);
            }
            s.timing_armed = armed;
            let row = (shown, armed, waiter, vblank);
            let c = Clocks { waiter, vblank };
            assert_eq!(s.stamp_source(c), source, "{row:?}");
            assert_eq!(s.stamps_estimated(c), estimated, "{row:?}");
            assert_eq!(s.stamps_active(c), active, "{row:?}");
        }
    }

    #[test]
    fn a_submit_consumes_the_held_image() {
        let mut s = SwapchainState::new();
        s.acquired = Some(2);
        s.on_submit();
        assert!(s.submitted);
        assert_eq!(s.acquired, None);
    }
}
