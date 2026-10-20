//! Linux xdg-ScreenCast/PipeWire and Windows IDD direct-push capturers, plus
//! synthetic test sources and the [`Capturer`] trait.
//!
//! Speaks [`pf_frame`] and the display leaves. Encode-backend facts arrive
//! pre-resolved in [`ZeroCopyPolicy`]; Windows sealed-section delivery arrives
//! as [`SetEncodeSender`] / [`CursorChannelSender`] closures. Never `pf-encode`
//! or the host orchestrator.
//!
//! Evidence: `design/idd-push-security.md`, `packaging/gamescope`.

// The frame vocabulary, at the crate root for every module's `crate::` imports.
#[cfg(any(target_os = "linux", target_os = "windows"))]
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};

mod capturer;
mod open;
mod synthetic;
pub use capturer::*;
pub use open::*;
pub use synthetic::{FastSyntheticCapturer, SyntheticCapturer};

/// Whether to ask a KWin output for delivery on its own frame signal.
///
/// KWin schedules each screencast frame on a QTimer whose wait it rounds *up* to a whole
/// millisecond, so an 8.333 ms frame is scheduled at 9 and the cadence jitters against the
/// real refresh. Unpaced, KWin up to 6.7 gets no ceiling and delivers on its damage
/// signal; KWin 6.8 (millihertz offer) gets the stream rate as its ceiling and paces the
/// cast at it. `PUNKTFUNK_KWIN_PACED=1` asks every KWin for the stream rate.
pub fn unpaced_capture() -> bool {
    !pf_host_config::row_bool("PUNKTFUNK_KWIN_PACED")
}

/// Offer PipeWire explicit sync (`SPA_META_SyncTimeline`) on the dmabuf lane.
///
/// A producer that takes it hands over a fence at each buffer's acquire point and waits on
/// the release point this side signals, instead of finishing the GPU itself. KWin on NVIDIA
/// `glFinish()`es its compositor thread per cast frame otherwise — ~9 ms under a game's
/// load, the whole 120 → 111 fps gap. `PUNKTFUNK_EXPLICIT_SYNC=0` keeps the implicit path.
pub fn explicit_sync() -> bool {
    pf_host_config::env_on("PUNKTFUNK_EXPLICIT_SYNC").unwrap_or(true)
}

/// Whether to capture a compositor's output directly with `ext-image-copy-capture-v1`
/// instead of going through the xdg ScreenCast portal.
///
/// The portal is a second clock in the path: xdg-desktop-portal-hyprland re-requests each
/// frame on a millisecond timer with a 6 ms floor, which halves the rate at 165 Hz and
/// adds ~3 ms to every frame's age. Capturing the protocol ourselves removes both.
/// `PUNKTFUNK_DIRECT_CAPTURE=0` keeps the portal.
#[cfg(target_os = "linux")]
pub fn direct_capture() -> bool {
    pf_host_config::row_bool("PUNKTFUNK_DIRECT_CAPTURE")
}

/// Whether a virtual output may be driven as a PipeWire lazy driver.
///
/// A producer that emits RequestProcess (Mutter ≥ 49 virtual monitors) paints only in a
/// graph cycle the consumer starts, so the encode loop's slot is the one tick: no
/// compositor timer to beat against, no throttle to lose frames in, no extra render.
/// Producers without it are never driven. `PUNKTFUNK_LAZY_CAPTURE=0` restores the
/// producer-driven stream.
#[cfg(target_os = "linux")]
pub fn lazy_capture() -> bool {
    pf_host_config::row_bool("PUNKTFUNK_LAZY_CAPTURE")
}

// The per-source HDR latch lives in pf-frame so pf-vdisplay reads it without this crate.
#[cfg(target_os = "linux")]
pub use pf_frame::hdr::{
    clear_virtual_output_hdr_latch, hdr_capture_failed, note_hdr_capture_failed, HdrSource,
};

// One-time PipeWire library init, shared by video (portal) and audio capture.
#[cfg(target_os = "linux")]
pub mod pwinit;

// Which clock the wire's `pts_ns` comes from. Linux-only consumer; arithmetic
// is platform-independent so tests run everywhere.
#[cfg(any(target_os = "linux", test))]
mod pts_provenance;

#[cfg(target_os = "windows")]
#[path = "windows/dxgi.rs"]
pub mod dxgi;
#[cfg(target_os = "windows")]
#[path = "windows/idd_push.rs"]
mod idd_push;
// What the win32u GPU-preference patch writes, and the one head it may write over. Pure
// byte tables, so the test runs on every target.
#[path = "windows/win32u_stub.rs"]
mod win32u_stub;
// The AU section's reader half. Pure over a mapped view, so its tests run on every target.
#[path = "windows/au_reader.rs"]
mod au_reader;
// The driver-encode proxy's chunk and AU assembly over that reader. Pure for the same reason.
#[path = "windows/au_assembly.rs"]
mod au_assembly;
// The recovery classifier's cursor-damage witness. Pure over `(now, kicked, position)` for the
// same reason: the rule that decides whether a silent desktop resets gets tests on every target.
#[path = "windows/cursor_witness.rs"]
mod cursor_witness;
// The IDD-push cursor render model: who draws the pointer and what the driver is told. Pure,
// so its table of past cursor defects runs on every target.
#[path = "windows/cursor_state.rs"]
mod cursor_state;
// The delivery gate and the frame shape of the two capturers that hand over no pixels. Pure.
#[path = "windows/pixelless.rs"]
mod pixelless;
// The IDD-push stall model: holes in DWM delivery and their verdicts. Pure over `Instant`s,
// so its tests run on every target; `idd_push/stall.rs` writes the report.
#[path = "windows/stall_model.rs"]
mod stall_model;
// The GDI cursor poller's planes-to-RGBA rasterisers, pure over `&[u8]` for the same reason.
#[path = "windows/cursor_raster.rs"]
mod cursor_raster;
#[cfg(target_os = "windows")]
pub use idd_push::driver_encode::{open_driver_encoder, DriverEncodeOpenError, DriverEncodeParams};
// The host's end of a capture worker's pipes. Generic over the pipes and the process, so its
// tests stand in for a worker on every target.
#[path = "windows/worker_link.rs"]
mod worker_link;
// A monitor the host did not create, captured and encoded by a capture worker.
#[cfg(target_os = "windows")]
#[path = "windows/wgc.rs"]
mod wgc;
#[cfg(target_os = "windows")]
pub use wgc::{open_wgc, open_worker_encoder, WgcSource, WorkerEndpoint, WorkerProcess};
#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
mod linux;
// GNOME BT.2100 colour-mode probe — host gate for offering HDR on the portal
// monitor path (`open_portal_monitor` `want_hdr`).
#[cfg(target_os = "linux")]
pub use linux::gnome_hdr_monitor_active;
#[cfg(target_os = "windows")]
#[path = "windows/synthetic_nv12.rs"]
pub mod synthetic_nv12;
