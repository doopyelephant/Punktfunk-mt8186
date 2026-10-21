//! The DXGI flip-model swapchain on the SDL window's HWND: three BGRA8 buffers, flip-discard,
//! a frame-latency waitable object at latency 1, tearing where the factory offers it.

use super::{dxgi_error, OwnedHandle};
use crate::vk::PresentPref;
use anyhow::{Context as _, Result};
use windows::core::Interface;
use windows::Win32::d3d11::{ID3D11Device5, ID3D11DeviceContext4, ID3D11RenderTargetView};
use windows::Win32::dxgi::{
    IDXGIFactory2, IDXGIFactory5, IDXGISwapChain2, DXGI_ALPHA_MODE_IGNORE,
    DXGI_FEATURE_PRESENT_ALLOW_TEARING, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT_ALLOW_TEARING, DXGI_SAMPLE_DESC, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING,
    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::synchapi::WaitForSingleObject;
use windows::Win32::windef::HWND;

/// `WaitForSingleObject`'s answer for a signalled object.
const WAIT_OBJECT_0: u32 = 0;

pub(super) struct Swapchain {
    chain: IDXGISwapChain2,
    /// The back buffer's view. Flip-model D3D11 renames buffer 0 to the current back buffer,
    /// so one view lives until the next resize.
    target: Option<ID3D11RenderTargetView>,
    extent: (u32, u32),
    /// Creation flags; `ResizeBuffers` must pass the same ones.
    flags: u32,
    tearing: bool,
    waitable: OwnedHandle,
    /// The waitable object was taken and no present has spent it yet.
    slot: bool,
}

impl Swapchain {
    pub(super) fn new(
        factory: &IDXGIFactory2,
        device: &ID3D11Device5,
        hwnd: HWND,
        extent: (u32, u32),
    ) -> Result<Swapchain> {
        let tearing = factory.cast::<IDXGIFactory5>().is_ok_and(|f| {
            let mut allowed = windows::core::BOOL(0);
            // SAFETY: a COM call on the live factory, writing one BOOL-sized local whose
            // size is the one passed.
            let hr = unsafe {
                f.CheckFeatureSupport(
                    DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                    std::ptr::from_mut(&mut allowed).cast(),
                    std::mem::size_of::<windows::core::BOOL>() as u32,
                )
            };
            hr.is_ok() && allowed.as_bool()
        });
        let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT as u32;
        if tearing {
            flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING as u32;
        }
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: extent.0.max(1),
            Height: extent.1.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE(DXGI_USAGE_RENDER_TARGET_OUTPUT),
            BufferCount: 3,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: flags,
            ..Default::default()
        };
        // SAFETY: COM calls on the live factory and device; `hwnd` is SDL's live window
        // handle and `desc` a local that outlives the call. The waitable object handle is
        // owned by the `OwnedHandle` built from it, which closes it once.
        let (chain, waitable) = unsafe {
            let chain = factory
                .CreateSwapChainForHwnd(device, hwnd, &desc, None, None)
                .context("create the DXGI swapchain")?;
            // Alt+Enter would take DXGI exclusive fullscreen; SDL owns fullscreen here.
            factory
                .MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER as u32)
                .ok()
                .context("MakeWindowAssociation")?;
            let chain: IDXGISwapChain2 = chain.cast().context("IDXGISwapChain2")?;
            chain
                .SetMaximumFrameLatency(1)
                .ok()
                .context("SetMaximumFrameLatency")?;
            let waitable = OwnedHandle(chain.GetFrameLatencyWaitableObject());
            (chain, waitable)
        };
        tracing::info!(
            width = desc.Width,
            height = desc.Height,
            tearing,
            "D3D11 swapchain"
        );
        Ok(Swapchain {
            chain,
            target: None,
            extent: (desc.Width, desc.Height),
            flags,
            tearing,
            waitable,
            slot: false,
        })
    }

    pub(super) fn extent(&self) -> (u32, u32) {
        self.extent
    }

    /// Resize the buffers to `extent`, keeping their count, format and flags. The back
    /// buffer's view goes first: DXGI refuses while anything references a buffer.
    pub(super) fn resize(
        &mut self,
        context: &ID3D11DeviceContext4,
        extent: (u32, u32),
    ) -> Result<()> {
        self.target = None;
        // SAFETY: COM calls on the live context and swapchain; no view of a back buffer is
        // bound or held once the targets are unbound and `target` is dropped above.
        unsafe {
            context.OMSetRenderTargets(None, None);
            context.Flush();
            self.chain
                .ResizeBuffers(0, extent.0, extent.1, DXGI_FORMAT_UNKNOWN, self.flags)
                .ok()
                .map_err(|e| dxgi_error(e, "resize the DXGI swapchain"))?;
        }
        self.extent = extent;
        Ok(())
    }

    /// Whether a present now would not block: the frame-latency object is signalled.
    /// A taken signal is kept until a present spends it.
    pub(super) fn ready(&mut self) -> bool {
        if !self.slot {
            // SAFETY: the waitable object is a live handle `self` owns; a zero timeout only
            // polls it.
            self.slot = unsafe { WaitForSingleObject(self.waitable.0, 0) } == WAIT_OBJECT_0;
        }
        self.slot
    }

    /// The back buffer's render-target view, made on first use after a create or resize.
    pub(super) fn target(&mut self, device: &ID3D11Device5) -> Result<ID3D11RenderTargetView> {
        if let Some(t) = &self.target {
            return Ok(t.clone());
        }
        let mut view = None;
        // SAFETY: COM calls on the live swapchain and device; the out-param is a local
        // `Option` checked below.
        unsafe {
            let buffer: windows::Win32::d3d11::ID3D11Texture2D = self
                .chain
                .GetBuffer(0)
                .map_err(|e| dxgi_error(e, "get the DXGI back buffer"))?;
            device
                .CreateRenderTargetView(&buffer, None, Some(&mut view))
                .ok()
                .map_err(|e| dxgi_error(e, "create the back buffer view"))?;
        }
        let view = view.context("CreateRenderTargetView returned no view")?;
        self.target = Some(view.clone());
        Ok(view)
    }

    /// Present with `mode`'s interval and flags; spends the waitable object's signal.
    pub(super) fn present(&mut self, mode: Mode) -> Result<()> {
        self.slot = false;
        // SAFETY: a COM call on the live swapchain; tearing flags are only passed when the
        // swapchain was created with `ALLOW_TEARING`.
        unsafe { self.chain.Present(mode.interval, mode.flags) }
            .ok()
            .map_err(|e| dxgi_error(e, "present the DXGI swapchain"))
    }

    pub(super) fn tearing(&self) -> bool {
        self.tearing
    }
}

/// A present's sync interval, flags and the name the stats line shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Mode {
    pub interval: u32,
    pub flags: u32,
    pub name: &'static str,
}

/// D12: `pinned` (the `present_mode` knob) picks the interval, `fifo` 1 and `immediate` or
/// `mailbox` 0; else V-Sync does. Interval 0 tears where the swapchain allows it and the
/// player allows variable refresh.
pub(super) fn mode(pref: PresentPref, pinned: Option<&str>, tearing: bool) -> Mode {
    let interval = match pinned {
        Some("fifo") => 1,
        Some("immediate" | "mailbox") => 0,
        _ => u32::from(pref.vsync),
    };
    match interval {
        1 => Mode {
            interval,
            flags: 0,
            name: "fifo",
        },
        _ if tearing && pref.allow_vrr => Mode {
            interval,
            flags: DXGI_PRESENT_ALLOW_TEARING,
            name: "tearing",
        },
        _ => Mode {
            interval,
            flags: 0,
            name: "immediate",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pref(vsync: bool, allow_vrr: bool) -> PresentPref {
        PresentPref {
            vsync,
            allow_vrr,
            ..Default::default()
        }
    }

    #[test]
    fn the_present_mode_follows_vsync_the_pin_and_tearing() {
        let rows: [(&str, PresentPref, Option<&str>, bool, &str); 5] = [
            ("vsync on is fifo", pref(true, true), None, true, "fifo"),
            (
                "vsync off tears where allowed",
                pref(false, true),
                None,
                true,
                "tearing",
            ),
            (
                "no tearing support is immediate",
                pref(false, true),
                None,
                false,
                "immediate",
            ),
            (
                "vrr refused is immediate",
                pref(false, false),
                None,
                true,
                "immediate",
            ),
            (
                "a fifo pin beats vsync off",
                pref(false, true),
                Some("fifo"),
                true,
                "fifo",
            ),
        ];
        for (name, p, pinned, tearing, want) in rows {
            assert_eq!(mode(p, pinned, tearing).name, want, "{name}");
        }
        assert_eq!(mode(pref(true, true), Some("mailbox"), true).interval, 0);
    }
}
