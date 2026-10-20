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
mod knobs;
mod open;
mod synthetic;
pub use capturer::*;
#[cfg(target_os = "linux")]
pub use knobs::direct_capture;
pub use open::*;
pub use synthetic::{FastSyntheticCapturer, SyntheticCapturer};

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

mod windows;
#[cfg(target_os = "windows")]
pub use self::windows::idd_push::driver_encode::DriverEncodeOpenError;
#[cfg(target_os = "windows")]
pub use self::windows::wgc::{open_wgc, WgcSource, WorkerProcess};
#[cfg(target_os = "windows")]
pub use self::windows::{gpu_pref, synthetic_nv12};
#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
mod linux;
// GNOME BT.2100 colour-mode probe — host gate for offering HDR on the portal
// monitor path (`open_portal_monitor` `want_hdr`).
#[cfg(target_os = "linux")]
pub use linux::gnome_hdr_monitor_active;
