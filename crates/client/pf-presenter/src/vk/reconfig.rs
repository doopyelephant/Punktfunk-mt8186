//! The reconfig model's performers: [`Presenter::apply`] runs the plan
//! `vk/reconfig_model.rs` decided — the swapchain's rebuild, the SDR↔HDR10 flip, the
//! present-mode pick, the lane's suspend and resume.

use super::setup::{pick_formats, pick_present_mode, PresentPref};
use super::{OverlayPipe, Presenter};
use crate::reconfig_model::{self as model, Plan};
use anyhow::{anyhow, Context as _, Result};
use ash::vk;

/// The model over this presenter's Vulkan types.
type Reconfig = model::Reconfig<vk::SurfaceFormatKHR, vk::PresentModeKHR>;
pub(super) type Input = model::Input<vk::SurfaceFormatKHR, vk::PresentModeKHR>;

/// What a performed plan leaves the frame that asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Applied {
    Done,
    /// The frame draws nothing: minimized, or a redraw the lane shows.
    Dropped,
}

/// NVIDIA proprietary still fails `vkCreateSwapchainKHR` after a successful
/// enumerate — it wants `vkAcquireDrmDisplayEXT`, which SDL's KMSDRM surface
/// does not. Empty on other backends. `drm_card` is the knob's raw value.
fn kmsdrm_swapchain_hint(drm_card: Option<&str>) -> String {
    let kmsdrm = std::env::var("SDL_VIDEODRIVER").is_ok_and(|v| v.eq_ignore_ascii_case("kmsdrm"));
    if !kmsdrm {
        return String::new();
    }
    let card = drm_card.unwrap_or("unset");
    format!(
        " — under SDL_VIDEODRIVER=kmsdrm (PUNKTFUNK_DRM_CARD={card}). Check, in order: the card \
         has a CONNECTED connector (`cat /sys/class/drm/card*-*/status`); nothing else holds DRM \
         master on it (a running compositor does — pin another card with PUNKTFUNK_DRM_CARD=<n>); \
         and the driver is Mesa. NVIDIA's proprietary direct-display path is known to fail here \
         even as root with a display Vulkan can enumerate."
    )
}

impl Presenter {
    /// Step the reconfig model on `input` and commit the state it answers. The caller
    /// performs the plan.
    pub(super) fn decide(&mut self, input: Input) -> Plan {
        let (next, plan) = model::step(&self.reconfig(), input);
        self.commit(next);
        plan
    }

    fn reconfig(&self) -> Reconfig {
        let p = self.pref;
        Reconfig {
            owner: self.owner,
            hdr_active: self.hdr_active,
            format: self.format,
            hdr10: self.hdr10_format,
            pref: model::Pref {
                vsync: p.vsync,
                allow_vrr: p.allow_vrr,
                fullscreen: p.fullscreen,
            },
            present_mode: self.present_mode,
            extent: (self.extent.width, self.extent.height),
        }
    }

    fn commit(&mut self, r: Reconfig) {
        self.owner = r.owner;
        self.hdr_active = r.hdr_active;
        self.format = r.format;
        self.hdr10_format = r.hdr10;
        self.pref = PresentPref {
            vsync: r.pref.vsync,
            allow_vrr: r.pref.allow_vrr,
            fullscreen: r.pref.fullscreen,
            ..self.pref
        };
        self.present_mode = r.present_mode;
        self.extent = vk::Extent2D {
            width: r.extent.0,
            height: r.extent.1,
        };
    }

    /// Step the model on `input`, then perform its plan.
    pub(super) fn reconfigure(
        &mut self,
        input: Input,
        window: &sdl3::video::Window,
    ) -> Result<Applied> {
        let plan = self.decide(input);
        self.apply(plan, window)
    }

    /// Perform `plan`. A performer feeds the model what only Vulkan answers.
    pub(super) fn apply(&mut self, plan: Plan, window: &sdl3::video::Window) -> Result<Applied> {
        match plan {
            Plan::Skip => {}
            Plan::Drop => return Ok(Applied::Dropped),
            Plan::PickMode(pref) => return self.pick_mode(window, pref),
            Plan::Flip { on } => return self.set_hdr_mode(window, on),
            Plan::Rebuild => self.rebuild_swapchain(window)?,
            #[cfg(target_os = "linux")]
            Plan::Resume => return self.resume_swapchain(window),
            #[cfg(target_os = "linux")]
            Plan::Suspend => self.suspend_swapchain()?,
            // Only the Linux lane takes the window.
            #[cfg(not(target_os = "linux"))]
            Plan::Resume | Plan::Suspend => {}
        }
        Ok(Applied::Done)
    }

    /// Re-pick the present mode for new V-Sync, VRR and fullscreen state. The swapchain is
    /// rebuilt only when the mode changes; call it between streams.
    pub fn set_present_pref(
        &mut self,
        window: &sdl3::video::Window,
        vsync: bool,
        allow_vrr: bool,
        fullscreen: bool,
    ) -> Result<()> {
        let pref = model::Pref {
            vsync,
            allow_vrr,
            fullscreen,
        };
        self.reconfigure(Input::SetPref(pref), window).map(drop)
    }

    /// `PickMode`: ask the surface for `pref`'s mode. A failed pick commits nothing.
    fn pick_mode(&mut self, window: &sdl3::video::Window, pref: model::Pref) -> Result<Applied> {
        let asked = PresentPref {
            vsync: pref.vsync,
            allow_vrr: pref.allow_vrr,
            fullscreen: pref.fullscreen,
            ..self.pref
        };
        let pinned = self.knobs.present_mode.as_deref();
        let mode = pick_present_mode(&self.surface_i, self.pdev, self.surface, asked, pinned)?;
        let plan = self.decide(Input::Picked { pref, mode });
        if plan == Plan::Rebuild {
            tracing::info!(present_mode = ?mode, vsync = pref.vsync, allow_vrr = pref.allow_vrr,
                fullscreen = pref.fullscreen, "present mode");
        }
        self.apply(plan, window)
    }

    /// The window may have a new size: rebuild the swapchain at it. While the native lane
    /// owns the window only the size is kept, for the resume.
    pub fn recreate_swapchain(&mut self, window: &sdl3::video::Window) -> Result<()> {
        let (width, height) = window.size_in_pixels();
        let resize = Input::Resize {
            extent: (width, height),
        };
        self.reconfigure(resize, window).map(drop)
    }

    /// `Rebuild`: replace the swapchain at the window's current size. A zero-size
    /// (minimized) window keeps the old swapchain and any image already acquired from it.
    fn rebuild_swapchain(&mut self, window: &sdl3::video::Window) -> Result<()> {
        self.quiesce_own()?;
        // SAFETY: `pdev` and `surface` are live handles owned by this presenter.
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut caps = unsafe {
            self.surface_i
                .get_physical_device_surface_capabilities(self.pdev, self.surface)
        }?;
        // An exclusive swapchain is sized and moded through its own chained queries.
        #[cfg(windows)]
        if let Some(f) = &self.fse {
            caps = f.capabilities(self.pdev, self.surface)?;
            let mode = f.present_mode(self.pdev, self.surface, self.present_mode);
            if mode != self.present_mode {
                tracing::info!(
                    requested = ?self.present_mode,
                    active = ?mode,
                    "exclusive fullscreen does not offer the present mode"
                );
                self.present_mode = mode;
            }
        }
        let (pw, ph) = window.size_in_pixels();
        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: pw.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: ph.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        };
        if extent.width == 0 || extent.height == 0 {
            // Minimized: keep the old swapchain. Presents return OUT_OF_DATE
            // and land back here once the window has a size.
            return Ok(());
        }
        // Presentation-engine semaphore waits finish here. A fence wait proves
        // only OUR submit (VUID-vkDestroySemaphore-05149 /
        // VUID-vkDestroySwapchainKHR-01282). Decode submits share `queue_lock`.
        {
            let _q = self.queue_lock.guard();
            // An image acquired ahead of a present belongs to the swapchain that goes now.
            if self.state.acquired.is_some() {
                // SAFETY: `queue_lock` is held above.
                unsafe { self.retire_acquire_sem() }
                    .context("vkQueueSubmit (discard the acquired image)")?;
                self.state.acquired = None;
            }
            // SAFETY: `queue` is owned here; `queue_lock` is held so no concurrent submit.
            unsafe { self.device.queue_wait_idle(self.queue) }
                .context("vkQueueWaitIdle (swapchain recreate)")?;
        }
        let mut min_images = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            min_images = min_images.min(caps.max_image_count);
        }

        // Drain before create: `oldSwapchain` is externally synchronised, so
        // the driver may retire it under a parked `vkWaitForPresentKHR`.
        // Bounded by the waiter's 250 ms cap.
        if let Some(t) = &self.present_timer {
            t.drain();
        }
        // Last present belonged to the dying swapchain. `note_presented` is
        // the only producer and shares this thread, so nothing enqueues
        // between this drain and the destroy below.
        self.last_presented = None;
        let old = self.swapchain;
        #[cfg(windows)]
        let mut fse_chain = Default::default();
        let mut info = vk::SwapchainCreateInfoKHR::default()
            .surface(self.surface)
            .min_image_count(min_images)
            .image_format(self.format.format)
            .image_color_space(self.format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            // TRANSFER_DST is clear + blit; COLOR_ATTACHMENT keeps the overlay
            // pass from forcing a swapchain-rebuild contract change.
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(self.present_mode)
            .clipped(true)
            .old_swapchain(old);
        // Present-id2/present-wait2 are asked for per swapchain, and so are engine stamps.
        let mut flags = vk::SwapchainCreateFlagsKHR::empty();
        if self.present_id2 {
            flags |= super::setup::present_wait2::SWAPCHAIN_FLAGS;
        }
        if self.timing.is_some() {
            flags |= super::timing_ext::SWAPCHAIN_FLAG;
        }
        info = info.flags(flags);
        #[cfg(windows)]
        if let Some(f) = &self.fse {
            info = f.extend(info, &mut fse_chain);
        }
        // SAFETY: `device`/`surface` are live; `info` is a local that outlives the call.
        let swapchain = unsafe { self.swap_d.create_swapchain(&info, None) }.map_err(|e| {
            anyhow!(
                "vkCreateSwapchainKHR: {e} ({}x{}, {:?} / {:?}, {:?}, {min_images} images){}",
                extent.width,
                extent.height,
                self.format.format,
                self.format.color_space,
                self.present_mode,
                kmsdrm_swapchain_hint(self.knobs.drm_card.as_deref())
            )
        })?;
        #[cfg(windows)]
        if let Some(f) = &self.fse {
            f.acquire(swapchain);
        }
        // SAFETY: quiesce covered our cmd bufs, the queue drain the presentation-engine
        // semaphore waits, the present-timer drain the last waiter: nothing still names
        // the targets, the semaphores or `old`.
        unsafe { self.destroy_targets(old) };
        self.swapchain = swapchain;
        // SAFETY: `swapchain` was created above; the waiter is drained, so this thread is
        // its only user.
        self.state.timing_armed = self
            .timing
            .as_ref()
            .is_some_and(|t| unsafe { t.arm(swapchain) });
        // SAFETY: `swapchain` was created above and is owned here.
        self.images = unsafe { self.swap_d.get_swapchain_images(swapchain) }?;
        self.extent = extent;
        self.overlay_pipe.rebuild_targets(
            &self.device,
            &self.images,
            self.format.format,
            extent,
        )?;

        for _ in 0..self.images.len() {
            // SAFETY: `device` is live; create-info is a local that outlives the call.
            self.render_sems.push(unsafe {
                self.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }?);
        }
        tracing::debug!(
            width = extent.width,
            height = extent.height,
            images = self.images.len(),
            "swapchain (re)created"
        );
        // HDR metadata is per-swapchain: a rebuilt HDR10 swapchain needs it
        // pushed again (`set_hdr_mode` into HDR10 also lands here).
        if self.hdr_active {
            self.apply_hdr_metadata();
        }
        Ok(())
    }

    /// The overlay targets, the render semaphores and `swapchain` (null is a no-op).
    ///
    /// # Safety
    /// Nothing in flight names them: our fence, the queue and the present waiter are drained.
    pub(super) unsafe fn destroy_targets(&mut self, swapchain: vk::SwapchainKHR) {
        self.overlay_pipe.destroy_targets(&self.device);
        // SAFETY: the caller's contract.
        unsafe {
            for s in self.render_sems.drain(..) {
                self.device.destroy_semaphore(s, None);
            }
            if swapchain != vk::SwapchainKHR::null() {
                self.swap_d.destroy_swapchain(swapchain, None);
            }
        }
    }

    /// Swapchain is HDR10/PQ, or the native lane hands PQ to the compositor with a PQ
    /// description; not a PQ stream tone-mapped onto SDR by us. User-facing "HDR"
    /// indicators should report this, not stream signalling.
    pub fn hdr_active(&self) -> bool {
        self.hdr_active || self.owner.native_pq()
    }

    /// The swapchain holds 10 bits a channel (SDR or HDR10): an overlay drawn in 8 would band.
    pub fn ten_bit(&self) -> bool {
        matches!(
            self.format.format,
            vk::Format::A2B10G10R10_UNORM_PACK32 | vk::Format::A2R10G10B10_UNORM_PACK32
        )
    }

    /// Drop back to the SDR swapchain while HDR10 is live on a swapchain with a size.
    ///
    /// Console UI is SDR and composites into whatever swapchain the last
    /// stream left. [`Presenter::present`] only flips on a frame with colour
    /// signalling; a UI-only present is `FrameInput::Redraw` (none). After a
    /// PQ session the UI would otherwise write into the HDR10 swapchain and
    /// sRGB mid-tones would emit as PQ code points.
    pub fn leave_hdr(&mut self, window: &sdl3::video::Window) -> Result<()> {
        let plan = self.decide(Input::LeaveHdr);
        if plan != Plan::Skip {
            tracing::info!("stream over — leaving HDR10 so the console UI composites as SDR");
        }
        self.apply(plan, window).map(drop)
    }

    /// Host ST.2086 + content-light from the 0xCE plane.
    pub fn set_hdr_metadata(&mut self, meta: punktfunk_core::quic::HdrMeta) {
        if self.hdr_meta == Some(meta) {
            return;
        }
        self.hdr_meta = Some(meta);
        if self.hdr_active {
            self.apply_hdr_metadata();
        }
    }

    /// `vkSetHdrMetadataEXT` with the host 0xCE values, or a generic HDR10
    /// baseline until that plane arrives. Compositors gate HDR-app signalling
    /// on this call — HDR10 colorspace alone leaves gamescope treating the
    /// app as SDR. No-op without the extension.
    fn apply_hdr_metadata(&self) {
        let Some(ext) = &self.hdr_metadata_d else {
            return;
        };
        // Suspended for the native lane: the rebuilt swapchain gets it pushed again.
        if self.swapchain == vk::SwapchainKHR::null() {
            return;
        }
        // Same generic baseline as the Windows presenter: BT.2020 + D65,
        // 1000-nit mastering, MaxCLL 1000 / MaxFALL 400.
        let m = self.hdr_meta.unwrap_or(punktfunk_core::quic::HdrMeta {
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 10_000_000,
            min_display_mastering_luminance: 1,
            max_cll: 1000,
            max_fall: 400,
        });
        // HDR10 SEI fixed-point: chromaticity 1/50000, luminance 0.0001
        // cd/m², primaries in ST.2086 G,B,R order. Vulkan wants 0..1 xy,
        // whole nits, primaries named R/G/B.
        let xy = |p: [u16; 2]| vk::XYColorEXT {
            x: p[0] as f32 / 50_000.0,
            y: p[1] as f32 / 50_000.0,
        };
        let [g, b, r] = m.display_primaries;
        let md = vk::HdrMetadataEXT::default()
            .display_primary_red(xy(r))
            .display_primary_green(xy(g))
            .display_primary_blue(xy(b))
            .white_point(xy(m.white_point))
            .max_luminance(m.max_display_mastering_luminance as f32 / 10_000.0)
            .min_luminance(m.min_display_mastering_luminance as f32 / 10_000.0)
            .max_content_light_level(m.max_cll as f32)
            .max_frame_average_light_level(m.max_fall as f32);
        // SAFETY: `swapchain` is live and owned here; `md` is a local.
        unsafe { ext.set_hdr_metadata(&[self.swapchain], &[md]) };
        tracing::debug!(from_host = self.hdr_meta.is_some(), "HDR metadata pushed");
    }
    /// `Flip`: SDR↔HDR10. The passes come first, then `Flipped` lands the format and the
    /// swapchain follows. The video intermediate ([`super::VIDEO_FORMAT`]) keeps its format;
    /// only its content, mapped for the old mode, is dropped. A driver that refuses the HDR10
    /// swapchain it advertised feeds `HdrRefused`: back to SDR for good, where PQ tone-maps.
    fn set_hdr_mode(&mut self, window: &sdl3::video::Window, on: bool) -> Result<Applied> {
        let target = if on {
            self.hdr10_format.expect("caller checked availability")
        } else {
            // `self.format` currently holds the HDR pairing; the SDR pick never changed.
            pick_formats(&self.surface_i, self.pdev, self.surface, false, &self.knobs)?.0
        };
        tracing::info!(hdr = on, format = ?target, "switching presentation mode");
        self.quiesce_own()?;
        if let Some(v) = self.video.take() {
            v.destroy(&self.device); // `quiesce_own` above: GPU idle on it
        }
        // The overlay, scale and direct passes render into the swapchain format. Build all
        // three first, then swap: a failed create leaves the old set whole, on one format.
        // The old views are only in our cmd bufs — the fence quiesce above makes the
        // destroy safe; the swapchain follows in the rebuild below.
        let mut new_pipe = OverlayPipe::new(&self.device, target.format, on)?;
        let mut new_scale = match crate::scale::ScalePass::new(&self.device, target.format) {
            Ok(s) => s,
            Err(e) => {
                new_pipe.destroy(&self.device);
                return Err(e);
            }
        };
        let new_direct = match crate::csc::DirectPass::new(
            &self.device,
            target.format,
            self.csc.pipeline_layout,
            self.csc_planar.pipeline_layout,
        ) {
            Ok(d) => d,
            Err(e) => {
                new_pipe.destroy(&self.device);
                new_scale.destroy(&self.device);
                return Err(e);
            }
        };
        std::mem::replace(&mut self.overlay_pipe, new_pipe).destroy(&self.device);
        std::mem::replace(&mut self.scale, new_scale).destroy(&self.device);
        std::mem::replace(&mut self.direct, new_direct).destroy(&self.device);
        self.cpu_direct = None;
        if let Some(h) = self.retired_hw.as_mut() {
            h.direct = None;
        }
        let flipped = Input::Flipped { on, format: target };
        match self.reconfigure(flipped, window) {
            Err(e) if on => {
                tracing::warn!(error = %format!("{e:#}"),
                    "HDR10 swapchain refused — staying SDR and tone-mapping PQ");
                self.reconfigure(Input::HdrRefused, window)
            }
            r => r,
        }
    }
}
