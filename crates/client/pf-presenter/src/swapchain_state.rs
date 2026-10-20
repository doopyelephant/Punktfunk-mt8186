//! The swapchain's per-frame bookkeeping beside the presenter's Vulkan handles: the submit
//! in flight, the image held between acquire and submit, the stamp asks, and which glass
//! clock stamps the display.
//!
//! Pure: no Vulkan. Who owns the window is the reconfig model's (`vk/reconfig_model.rs`).
//! Pinned by the table tests below. The order inside a present (import, acquire, retire)
//! is `vk/present.rs`'s.

/// The glass clocks that follow the swapchain's presents.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clocks {
    /// Present-wait, either generation.
    pub(crate) waiter: bool,
    /// The vblank waiter, whose stamps are estimates.
    pub(crate) vblank: bool,
    /// The last frame went through the native lane, whose feedback stamps it.
    pub(crate) lane: bool,
}

pub(crate) struct SwapchainState {
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
            submitted: false,
            acquired: None,
            timing_armed: false,
            timing_asked: false,
        }
    }

    /// A submit waited the held image's `acquire_sem` and named the fence.
    pub(crate) fn on_submit(&mut self) {
        self.submitted = true;
        self.acquired = None;
    }

    /// Where the display stamp comes from: `feedback` (the lane), `timing` (the engine's
    /// own stamps), `wait` (present-wait), `est` (the vblank waiter), `none`.
    pub(crate) fn stamp_source(&self, c: Clocks) -> &'static str {
        if c.lane {
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
        !c.waiter && c.vblank && !c.lane
    }

    /// Present-wait, the vblank waiter or the lane's feedback drives the display stamp.
    pub(crate) fn stamps_active(&self, c: Clocks) -> bool {
        c.waiter || c.vblank || c.lane
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            s.timing_armed = armed;
            let row = (shown, armed, waiter, vblank);
            let c = Clocks {
                waiter,
                vblank,
                lane: shown,
            };
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
