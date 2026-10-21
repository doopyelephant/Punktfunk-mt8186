//! The D3D11 presenter: one D3D11.4 device on the DXGI adapter the knobs name, a flip-model
//! swapchain on the SDL window's HWND, and the CSC pass that draws a frame placed by
//! `video_fit` with the bars cleared. `backend` picks it on Windows where Vulkan cannot
//! import D3D11 (planning `design/windows-d3d11-presenter.md`).
//!
//! Frames: `FrameInput::Cpu` (three R8 planes, `frames`) and `Redraw` (the last planes drawn
//! again); every other input is an error, which the run loop's health policy demotes.
//! The device is the decoder's too: [`Presenter::vulkan_decode`] hands it over in the facts
//! with `d3d11_present` set and every Vulkan handle zero.
//! Not built here: the glass instrument (the timing readers answer as a presenter with no
//! stamps), HDR10 (the swapchain is BGRA8 and PQ is tonemapped), the scale ladder (the CSC
//! samples bilinear; `Kernel` is logged at debug) and the console overlay (no Vulkan device).
//! Errors: a removed or reset device is `ash::vk::Result::ERROR_DEVICE_LOST` in the chain,
//! the one `run/` ends the session on; swapchain failures carry `PresentError::Window`.

mod csc;
mod frames;
mod swapchain;

use crate::knobs::Knobs;
use crate::overlay::OverlayFrame;
use crate::vk::{BusyOn, FrameInput, PresentPref, Presented, PresentedSample};
use anyhow::{anyhow, bail, Context as _, Result};
use pf_client_core::video::{ColorDesc, VulkanDecodeDevice};
use punktfunk_core::video_fit::{self, VideoFit};
use windows::core::Interface;
use windows::Win32::d3d11::{
    D3D11CreateDevice, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_FENCE_FLAG_NONE,
    D3D11_SDK_VERSION,
};
use windows::Win32::d3dcommon::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIFactory2, IDXGIFactory6,
    DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
};
use windows::Win32::winerror::{DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET};
use windows::Win32::winnt::HANDLE;

/// DXGI's vendor id for Qualcomm ("QCOM"). The facts carry Vulkan's PCI-style 0x5143
/// instead, the id every vendor rule in the decode ladder keys on.
const DXGI_VENDOR_QUALCOMM: u32 = 0x4D4F_4351;
const VENDOR_QUALCOMM: u32 = 0x5143;

/// The PQ→SDR peak when `knobs.tonemap_peak` is unset, as the Vulkan CSC: ≈ 1000 nits.
const TONEMAP_PEAK: f32 = 4.9;

/// A kernel handle closed on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0 .0.is_null() {
            // SAFETY: `self.0` is a handle this value owns; `Drop` runs once, so it is closed
            // exactly once and never used after.
            let _ = unsafe { windows::Win32::handleapi::CloseHandle(self.0) };
        }
    }
}

/// A failed DXGI or D3D11 call as `run/` classes it: a removed or reset device is the lost
/// device that ends the session, anything else the window's.
fn dxgi_error(e: windows::core::Error, what: &'static str) -> anyhow::Error {
    let code = e.code();
    if code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET {
        anyhow::Error::new(ash::vk::Result::ERROR_DEVICE_LOST).context(format!("{what}: {e}"))
    } else {
        anyhow::Error::new(e)
            .context(crate::vk::PresentError::Window)
            .context(what)
    }
}

pub(crate) struct Presenter {
    device: ID3D11Device5,
    context: ID3D11DeviceContext4,
    swapchain: swapchain::Swapchain,
    csc: csc::Csc,
    /// The software lane's planes and the colour of what they hold; a `Redraw` draws them
    /// again. `None` until the first CPU frame and after `drop_video`.
    planes: Option<frames::CpuPlanes>,
    shown: Option<ColorDesc>,
    facts: VulkanDecodeDevice,
    /// `wait_idle`'s fence, its last value, and the event its completion sets.
    idle_fence: ID3D11Fence,
    idle_value: std::cell::Cell<u64>,
    idle_event: OwnedHandle,
    pref: PresentPref,
    knobs: Knobs,
    video_fit: VideoFit,
    kernel_logged: bool,
    /// Held for HDR10, which this backend does not present yet.
    _hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
    /// Held for the frame-latency waiter, which this backend does not run yet.
    _wake: std::cell::Cell<Option<Box<dyn Fn() + Send>>>,
}

impl Presenter {
    /// Device, swapchain and CSC pipeline over `window`'s HWND. `pref` and `knobs` choose
    /// the present interval and the adapter (`vk_adapter`, matched against DXGI's names).
    pub(crate) fn new(
        window: &sdl3::video::Window,
        pref: PresentPref,
        knobs: &Knobs,
    ) -> Result<Presenter> {
        let hwnd = crate::win32::window_hwnd(window).context("the SDL window has no HWND")?;
        let hwnd = windows::Win32::windef::HWND(hwnd);
        // SAFETY: factory creation takes no pointer and returns an owned factory or an error.
        let factory: IDXGIFactory1 =
            unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
        let adapters = adapters(&factory);
        let names: Vec<(String, bool)> = adapters
            .iter()
            .map(|(_, d)| {
                (
                    adapter_name(d),
                    d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE as u32 != 0,
                )
            })
            .collect();
        let pick = crate::backend::pick_adapter(&names, knobs.vk_adapter.as_deref())
            .ok_or_else(|| anyhow!("no hardware DXGI adapter"))?;
        let (adapter, desc) = &adapters[pick];
        let (device, context) = create_device(adapter)?;
        let factory2: IDXGIFactory2 = factory.cast().context("IDXGIFactory2")?;
        let swapchain =
            swapchain::Swapchain::new(&factory2, &device, hwnd, window.size_in_pixels())?;
        let csc = csc::Csc::new(&device)?;
        let mut idle_fence: Option<ID3D11Fence> = None;
        // SAFETY: COM call on the live device; the out-param is a local checked below. The
        // event handle is owned by the `OwnedHandle` built from it.
        let idle_event = unsafe {
            device
                .CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut idle_fence)
                .context("create the idle fence")?;
            OwnedHandle(windows::Win32::synchapi::CreateEventW(
                None,
                false,
                false,
                windows::core::PCWSTR::null(),
            ))
        };
        if idle_event.0 .0.is_null() {
            bail!("create the idle event");
        }
        let facts = decode_facts(&device, desc);
        tracing::info!(
            adapter = %facts.device_name,
            vendor_id = format_args!("0x{:04X}", facts.vendor_id),
            "D3D11 presenter"
        );
        Ok(Presenter {
            device,
            context,
            swapchain,
            csc,
            planes: None,
            shown: None,
            facts,
            idle_fence: idle_fence.context("CreateFence returned none")?,
            idle_value: std::cell::Cell::new(0),
            idle_event,
            pref,
            knobs: knobs.clone(),
            video_fit: VideoFit::default(),
            kernel_logged: false,
            _hdr_meta: None,
            _wake: std::cell::Cell::new(None),
        })
    }

    /// Present one frame. `Busy` hands the frame back unconsumed while the frame-latency
    /// object is unsignalled, so the run loop retries instead of blocking in `Present`.
    pub(crate) fn present<'a>(
        &mut self,
        window: &sdl3::video::Window,
        input: FrameInput<'a>,
        _overlay: Option<&OverlayFrame>,
    ) -> Result<Presented<'a>> {
        let frame = match &input {
            FrameInput::Redraw => None,
            FrameInput::Cpu(f) => Some(*f),
            _ => bail!("the D3D11 presenter takes no Vulkan or dma-buf frames"),
        };
        let extent = window.size_in_pixels();
        if extent.0 == 0 || extent.1 == 0 {
            return Ok(Presented::Shown); // minimized: nothing would show
        }
        if extent != self.swapchain.extent() {
            self.swapchain.resize(&self.context, extent)?;
        }
        if !self.swapchain.ready() {
            return Ok(Presented::Busy(input, BusyOn::Acquire));
        }
        if let Some(f) = frame {
            frames::CpuPlanes::upload(&mut self.planes, &self.device, &self.context, f)?;
            self.shown = Some(f.color);
        }
        let target = self.swapchain.target(&self.device)?;
        // SAFETY: COM calls on the live immediate context with the back buffer's live view.
        unsafe {
            self.context
                .OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            self.context
                .ClearRenderTargetView(&target, &[0.0, 0.0, 0.0, 1.0]);
        }
        if let (Some(planes), Some(color)) = (&self.planes, self.shown) {
            let place = video_fit::place(self.video_fit, extent, planes.size());
            if !place.is_empty() {
                if !std::mem::replace(&mut self.kernel_logged, true) {
                    tracing::debug!(
                        kernel = ?(place.kernel_x().name(), place.kernel_y().name()),
                        "D3D11 presenter scales bilinear, not by the kernel"
                    );
                }
                let peak = self.knobs.tonemap_peak.unwrap_or(TONEMAP_PEAK);
                let tonemap = color.is_pq().then_some(peak);
                self.csc.draw_planar(
                    &self.context,
                    &planes.views,
                    planes.size(),
                    &place,
                    color,
                    tonemap,
                );
            }
        }
        self.swapchain.present(self.mode())?;
        Ok(Presented::Shown)
    }

    fn mode(&self) -> swapchain::Mode {
        swapchain::mode(
            self.pref,
            self.knobs.present_mode.as_deref(),
            self.swapchain.tearing(),
        )
    }

    /// The window may have a new size: resize the buffers to it.
    pub(crate) fn recreate_swapchain(&mut self, window: &sdl3::video::Window) -> Result<()> {
        let extent = window.size_in_pixels();
        if extent.0 == 0 || extent.1 == 0 || extent == self.swapchain.extent() {
            return Ok(());
        }
        self.swapchain.resize(&self.context, extent)
    }

    /// New V-Sync, VRR and fullscreen state; the next present takes its interval from it.
    pub(crate) fn set_present_pref(&mut self, vsync: bool, allow_vrr: bool, fullscreen: bool) {
        self.pref = PresentPref {
            vsync,
            allow_vrr,
            fullscreen,
            ..self.pref
        };
    }

    pub(crate) fn set_video_fit(&mut self, fit: VideoFit) {
        self.video_fit = fit;
        self.kernel_logged = false;
    }

    /// The device facts the decoder builds on; see the module map.
    pub(crate) fn vulkan_decode(&self) -> Option<VulkanDecodeDevice> {
        Some(self.facts.clone())
    }

    /// Flush the context and wait for its work. Teardown only; a removed device never
    /// completes the fence, so the wait gives up after a second.
    pub(crate) fn wait_idle(&self) {
        let value = self.idle_value.get() + 1;
        self.idle_value.set(value);
        // SAFETY: COM calls on the live context and fence; the event is a live handle this
        // presenter owns, waited with a bounded timeout.
        unsafe {
            if self.context.Signal(&self.idle_fence, value).is_err() {
                return;
            }
            self.context.Flush();
            if self
                .idle_fence
                .SetEventOnCompletion(value, self.idle_event.0)
                .is_ok()
            {
                windows::Win32::synchapi::WaitForSingleObject(self.idle_event.0, 1000);
            }
        }
    }

    /// Let go of the stream's picture: the planes go, and a `Redraw` clears to black.
    pub(crate) fn drop_video(&mut self) {
        self.planes = None;
        self.shown = None;
    }

    pub(crate) fn set_hdr_metadata(&mut self, meta: punktfunk_core::quic::HdrMeta) {
        self._hdr_meta = Some(meta);
    }

    pub(crate) fn set_present_wake(&self, cb: Box<dyn Fn() + Send>) {
        self._wake.set(Some(cb));
    }

    pub(crate) fn present_mode_name(&self) -> &'static str {
        self.mode().name
    }

    /// No display stamps: the run loop's submit-time fallback holds, as on a Vulkan
    /// presenter without present-wait.
    pub(crate) fn take_presented_samples(&mut self) -> Vec<PresentedSample> {
        Vec::new()
    }
}

/// Every adapter DXGI lists, high-performance first where the factory can order them.
fn adapters(factory: &IDXGIFactory1) -> Vec<(IDXGIAdapter1, DXGI_ADAPTER_DESC1)> {
    let by_preference = factory.cast::<IDXGIFactory6>().ok();
    (0..)
        .map_while(|i| {
            // SAFETY: COM calls on the live factory; `ok()` keeps only an adapter that came back.
            unsafe {
                match &by_preference {
                    Some(f) => f
                        .EnumAdapterByGpuPreference::<IDXGIAdapter1>(
                            i,
                            DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
                        )
                        .ok(),
                    None => factory.EnumAdapters1(i).ok(),
                }
            }
        })
        .filter_map(|adapter| {
            let mut desc = DXGI_ADAPTER_DESC1::default();
            // SAFETY: a COM call on the adapter just enumerated, filling the local descriptor.
            let described = unsafe { adapter.GetDesc1(&mut desc) }.is_ok();
            described.then_some((adapter, desc))
        })
        .collect()
}

fn adapter_name(desc: &DXGI_ADAPTER_DESC1) -> String {
    let len = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    String::from_utf16_lossy(&desc.Description[..len])
}

/// A D3D11.4 device and immediate context on `adapter`, decode-capable, with multithread
/// protection on: the decoder calls into this device from the pump thread.
fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device5, ID3D11DeviceContext4)> {
    let (mut device, mut context) = (None, None);
    // SAFETY: `adapter` is live; the two out-params are local `Option`s the callee only writes,
    // both checked before use.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            windows::Win32::minwindef::HINSTANCE(std::ptr::null_mut()),
            (D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT) as u32,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION as u32,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .ok()
    .context("D3D11CreateDevice")?;
    let device = device.context("D3D11CreateDevice returned no device")?;
    let context = context.context("D3D11CreateDevice returned no context")?;
    let device: ID3D11Device5 = device.cast().context("D3D11.4 device")?;
    let context: ID3D11DeviceContext4 = context.cast().context("D3D11.4 device")?;
    let mt: ID3D11Multithread = context.cast().context("ID3D11Multithread")?;
    // SAFETY: a COM call on the live interface from a checked cast; the previous state it
    // returns is not needed.
    let _ = unsafe { mt.SetMultithreadProtected(true) };
    Ok((device, context))
}

/// The facts for the decoder: this device, the adapter's vendor, name and LUID, and no
/// Vulkan device (every handle zero, no extensions, no Vulkan decode lanes).
fn decode_facts(device: &ID3D11Device5, desc: &DXGI_ADAPTER_DESC1) -> VulkanDecodeDevice {
    let mut luid = [0u8; 8];
    luid[..4].copy_from_slice(&desc.AdapterLuid.LowPart.to_le_bytes());
    luid[4..].copy_from_slice(&desc.AdapterLuid.HighPart.to_le_bytes());
    let vendor_id = match desc.VendorId {
        DXGI_VENDOR_QUALCOMM => VENDOR_QUALCOMM,
        v => v,
    };
    VulkanDecodeDevice {
        get_instance_proc_addr: 0,
        instance: 0,
        physical_device: 0,
        device: 0,
        vendor_id,
        device_name: adapter_name(desc),
        graphics_qf: 0,
        decode_qf: 0,
        decode_video_caps: 0,
        instance_extensions: Vec::new(),
        device_extensions: Vec::new(),
        f_sampler_ycbcr: false,
        f_timeline_semaphore: false,
        f_synchronization2: false,
        f_shader_int16: false,
        f_storage_buffer8: false,
        f_subgroup_size_control: false,
        f_compute_full_subgroups: false,
        f_shader_float16: false,
        api_version: 0,
        queue_families: Vec::new(),
        pyrowave_decode: false,
        video_decode: false,
        d3d11_present: true,
        d3d11_fence: false,
        d3d11_device: Some(pf_client_core::video::video_vk::D3d11DeviceHandle::new(
            device.clone().into(),
        )),
        d3d11_import: false,
        dmabuf_import: false,
        vaapi_av1_decode: false,
        vaapi_hevc_decode: false,
        d3d11_hdr10: false,
        d3d11_nv12: false,
        d3d11_p010: false,
        adapter_luid: Some(luid),
        queue_lock: std::sync::Arc::new(pf_client_core::video::QueueLock::new()),
    }
}
