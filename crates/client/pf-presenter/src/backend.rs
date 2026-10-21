//! The presenter `run/` draws through: one enum over the backends, one `impl` forwarding
//! every call the shell makes. The frame, outcome and timing types stay in `vk`.

use crate::overlay::{OverlayFrame, SharedDevice};
use crate::vk::{FrameInput, NativeVkOutcome, Presented, PresentedSample};
use anyhow::Result;
use pf_client_core::video::{NativeVkFrame, VulkanDecodeDevice};
use sdl3::video::Window;

pub(crate) enum Presenter {
    Vk(crate::vk::Presenter),
}

/// The forwarded call on whichever backend `$self` holds.
macro_rules! each {
    ($self:expr, $p:ident => $call:expr) => {
        match $self {
            Presenter::Vk($p) => $call,
        }
    };
}

impl Presenter {
    /// The Vulkan presenter over `window`, which SDL built with its Vulkan flag.
    pub(crate) fn new(
        window: &Window,
        pref: crate::vk::PresentPref,
        knobs: &crate::knobs::Knobs,
    ) -> Result<Presenter> {
        use anyhow::Context as _;
        let exts = window
            .vulkan_instance_extensions()
            .map_err(|e| anyhow::anyhow!("vulkan instance extensions: {e}"))?;
        let p =
            crate::vk::Presenter::new(window, &exts, pref, knobs).context("vulkan presenter")?;
        Ok(Presenter::Vk(p))
    }

    pub(crate) fn present<'a>(
        &mut self,
        window: &Window,
        input: FrameInput<'a>,
        overlay: Option<&OverlayFrame>,
    ) -> Result<Presented<'a>> {
        each!(self, p => p.present(window, input, overlay))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn present_native(
        &mut self,
        d: pf_client_core::video::DmabufFrame,
        pts_ns: u64,
        decoded_ns: u64,
    ) -> crate::wl_native::Outcome {
        each!(self, p => p.present_native(d, pts_ns, decoded_ns))
    }

    pub(crate) fn present_native_vk(
        &mut self,
        f: NativeVkFrame,
        pts_ns: u64,
        decoded_ns: u64,
    ) -> NativeVkOutcome {
        each!(self, p => p.present_native_vk(f, pts_ns, decoded_ns))
    }

    #[cfg(feature = "pyrowave")]
    pub(crate) fn present_native_pyro(
        &mut self,
        f: pf_client_core::video_pyrowave::PyroWavePlanarFrame,
        pts_ns: u64,
        decoded_ns: u64,
    ) -> NativeVkOutcome<pf_client_core::video_pyrowave::PyroWavePlanarFrame> {
        each!(self, p => p.present_native_pyro(f, pts_ns, decoded_ns))
    }

    pub(crate) fn recreate_swapchain(&mut self, window: &Window) -> Result<()> {
        each!(self, p => p.recreate_swapchain(window))
    }

    pub(crate) fn set_present_pref(
        &mut self,
        window: &Window,
        vsync: bool,
        allow_vrr: bool,
        fullscreen: bool,
    ) -> Result<()> {
        each!(self, p => p.set_present_pref(window, vsync, allow_vrr, fullscreen))
    }

    pub(crate) fn sync_native_overlay(
        &mut self,
        overlay: Option<&OverlayFrame>,
        logical: (u32, u32),
    ) {
        each!(self, p => p.sync_native_overlay(overlay, logical))
    }

    pub(crate) fn set_video_fit(&mut self, fit: punktfunk_core::video_fit::VideoFit) {
        each!(self, p => p.set_video_fit(fit))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn supports_dmabuf(&self) -> bool {
        each!(self, p => p.supports_dmabuf())
    }

    #[cfg(windows)]
    pub(crate) fn supports_d3d11(&self) -> bool {
        each!(self, p => p.supports_d3d11())
    }

    /// The overlay's device; `None` where the backend has no Vulkan device to share.
    pub(crate) fn shared_device(&self) -> Option<SharedDevice> {
        each!(self, p => Some(p.shared_device()))
    }

    pub(crate) fn vulkan_decode(&self) -> Option<VulkanDecodeDevice> {
        each!(self, p => p.vulkan_decode())
    }

    pub(crate) fn begin_present_turn(&self, turn: u64) {
        each!(self, p => p.begin_present_turn(turn))
    }

    pub(crate) fn end_present_turn(&self, turn: u64) {
        each!(self, p => p.end_present_turn(turn))
    }

    pub(crate) fn wait_idle(&self) {
        each!(self, p => p.wait_idle())
    }

    pub(crate) fn drop_video(&mut self) {
        each!(self, p => p.drop_video())
    }

    pub(crate) fn hdr_active(&self) -> bool {
        each!(self, p => p.hdr_active())
    }

    pub(crate) fn ten_bit(&self) -> bool {
        each!(self, p => p.ten_bit())
    }

    pub(crate) fn leave_hdr(&mut self, window: &Window) -> Result<()> {
        each!(self, p => p.leave_hdr(window))
    }

    pub(crate) fn set_hdr_metadata(&mut self, meta: punktfunk_core::quic::HdrMeta) {
        each!(self, p => p.set_hdr_metadata(meta))
    }

    pub(crate) fn present_timing_active(&self) -> bool {
        each!(self, p => p.present_timing_active())
    }

    pub(crate) fn glass_estimated(&self) -> bool {
        each!(self, p => p.glass_estimated())
    }

    pub(crate) fn measured_refresh_ns(&self) -> Option<u64> {
        each!(self, p => p.measured_refresh_ns())
    }

    pub(crate) fn engine_refresh(&self) -> Option<(u64, u64)> {
        each!(self, p => p.engine_refresh())
    }

    pub(crate) fn glass_source(&self) -> &'static str {
        each!(self, p => p.glass_source())
    }

    pub(crate) fn retarget_glass(&self, window: &Window) {
        each!(self, p => p.retarget_glass(window))
    }

    pub(crate) fn take_unshown(&self) -> u32 {
        each!(self, p => p.take_unshown())
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn take_scanout(&mut self) -> Option<(u32, u32)> {
        each!(self, p => p.take_scanout())
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn take_native_zero_copy(&mut self) -> (u32, u32) {
        each!(self, p => p.take_native_zero_copy())
    }

    pub(crate) fn note_presented(&mut self, pts_ns: u64, decoded_ns: u64) {
        each!(self, p => p.note_presented(pts_ns, decoded_ns))
    }

    pub(crate) fn presents_outstanding(&self) -> usize {
        each!(self, p => p.presents_outstanding())
    }

    pub(crate) fn set_present_wake(&self, cb: Box<dyn Fn() + Send>) {
        each!(self, p => p.set_present_wake(cb))
    }

    pub(crate) fn last_timings(&self) -> (u32, u32) {
        each!(self, p => p.last_timings())
    }

    pub(crate) fn last_waits(&self) -> (u32, u32, u32) {
        each!(self, p => p.last_waits())
    }

    pub(crate) fn present_mode_name(&self) -> &'static str {
        each!(self, p => p.present_mode_name())
    }

    pub(crate) fn needs_glass_gate(&self) -> bool {
        each!(self, p => p.needs_glass_gate())
    }

    pub(crate) fn vblank_locked(&self) -> bool {
        each!(self, p => p.vblank_locked())
    }

    pub(crate) fn take_presented_samples(&mut self) -> Vec<PresentedSample> {
        each!(self, p => p.take_presented_samples())
    }
}
