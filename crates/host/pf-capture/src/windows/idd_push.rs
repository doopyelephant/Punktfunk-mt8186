//! Host-side IDD-push capture: the driver encodes, so this side owns no pixels.
//!
//! The driver's swap-chain worker passes each composed frame through its encode
//! pool into a sealed AU section ([`crate::windows::remote::encoder`]); this capturer owns
//! the session's display identity — mode, HDR, cursor channel — and wraps the
//! [`RemoteSource`] that reports the driver's frame cadence as pixel-less
//! [`CapturedFrame`]s and supervises it. Driver:
//! `packaging/windows/drivers/pf-vdisplay/src/encode/`. Layout and status codes
//! live in [`pf_driver_proto`] — both sides `use` it, so drift is a compile error.

use crate::knobs::Knobs;
use crate::windows::cursor_state::CursorModel;
use crate::windows::cursor_witness::CursorWitness;
use crate::windows::remote::section::{MappedSection, SECTION_MAP_RW};
use crate::windows::remote::source::RemoteSource;
use crate::{CapturedFrame, Capturer, PixelFormat};
use anyhow::{bail, Context, Result};
use pf_frame::dxgi::WinCaptureTarget;
use pf_win_display::open_wudfhost;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    DuplicateHandle, DUPLICATE_CLOSE_SOURCE, DUPLICATE_HANDLE_OPTIONS, DUPLICATE_SAME_ACCESS,
    HANDLE, POINT, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, WaitForSingleObject, PROCESS_SET_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

/// A session found SDR wide colour refused on this host's virtual display. Later handshakes
/// then stop offering a 10-bit SDR source without the widening opt-in.
static WCG_REFUSED: AtomicBool = AtomicBool::new(false);

/// A 10-bit SDR session can compose in SDR wide colour: Windows 11 24H2, and no refusal on
/// this host yet. Before the first session tries, the OS build alone answers.
pub(crate) fn wcg_available() -> bool {
    static BUILD: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    !WCG_REFUSED.load(Ordering::Relaxed)
        && *BUILD.get_or_init(pf_win_display::os_build) >= pf_win_display::WCG_MIN_BUILD
}

/// Where the access-unit dump goes while [`Knobs::idd_diag`] is on: this capturer's full
/// diagnostics.
pub(super) fn diag_dir() -> Option<&'static std::path::Path> {
    Knobs::process().idd_diag.as_deref()
}

/// [`Knobs::idd_flow`]: the DxgKrnl ETW session alone. For a field host: [`diag_dir`] also
/// dumps every access unit to disk.
pub(super) fn flow_watch() -> bool {
    Knobs::process().idd_flow
}

// The frame-delivery endpoint: `try_consume` and the `Capturer` surface.
#[path = "idd_push/capturer.rs"]
mod capturer;
#[path = "idd_push/channel.rs"]
mod channel;
// Construction: adapter, HDR, cursor opt-in.
#[path = "idd_push/cursor.rs"]
mod cursor;
#[path = "idd_push/cursor_model.rs"]
mod cursor_model;
#[path = "idd_push/cursor_poll.rs"]
mod cursor_poll;
#[path = "idd_push/open.rs"]
mod open;
use cursor_model::deliver_cursor_channel;
#[path = "idd_push/descriptor.rs"]
mod descriptor;
#[path = "idd_push/display.rs"]
mod display;
// Stall reporting over `windows::stall_model`, plus DxgKrnl ETW and micro-probes as evidence.
#[path = "idd_push/dxgkrnl_etw.rs"]
mod dxgkrnl_etw;
#[path = "idd_push/probes.rs"]
mod probes;
#[path = "idd_push/stall.rs"]
mod stall;
// This capturer's end of the recovery ladder: the classifier inputs and the rungs it runs here.
#[path = "idd_push/health.rs"]
mod health;
use crate::windows::stall_model::{StallEvidence, StallWatch};
use channel::{ChannelBroker, IddTransport};
use descriptor::{DescriptorPoller, DisplayDescriptor};

/// The session's virtual display: its mode, its cursor channel, and the remote source that
/// delivers and supervises it.
///
/// Frames carry no pixels — the driver encodes them — so a delivery is only the
/// news that the driver's pool took a new composed frame, which is what the
/// stream loop needs to pace and what the classifier needs as source progress.
pub struct IddPushCapturer {
    /// Driver-protocol target id (encoder open, cursor channel, logs). CCD path selection goes
    /// through `ccd` — a bare id is only unique per adapter.
    target_id: u32,
    /// Complete CCD identity (adapter LUID + target id) for every display-global helper.
    ccd: pf_win_display::win_display::CcdTargetKey,
    /// The delivery gate, the supervisor and the encoder's telemetry, over the driver's
    /// transport. The in-place resize waits for a delivery at the new size with the encoder
    /// untouched.
    remote: RemoteSource<IddTransport>,
    /// Handle duplication into WUDFHost for the cursor channel; the transport shares it.
    broker: Arc<ChannelBroker>,
    /// Hardware-cursor shm (`Some` = delivered). The driver publishes into it and blends
    /// from it; this side reads it for the client's own pointer.
    cursor_shared: Option<cursor::CursorShared>,
    /// GDI pointer position and visibility while alive, and the shape for a cursor the
    /// section holds none of.
    cursor_poll: Option<cursor_poll::CursorPoller>,
    /// Whether the client's last shape came from the section; the first and every change
    /// are logged.
    shape_from_section: Option<bool>,
    /// `IOCTL_SET_CURSOR_FORWARD`. A declared IddCx hardware cursor blocks the
    /// OS software-cursor path; [`Self::poll_secure_desktop`] stands it down at UAC/Winlogon.
    cursor_forward: Option<crate::CursorForwardSender>,
    /// Kept so a re-arrived monitor can be handed the cursor channel again. The driver's worker
    /// does not survive a re-arrival, and the composite render model has no other shape source.
    cursor_sender: Option<crate::CursorChannelSender>,
    /// Who draws the pointer, whether the secure desktop has the declare stood down, and
    /// which source the client's shape is read from.
    cursor_model: CursorModel,
    /// HDR cursor match to desktop SDR white (vs 80 nits). 2.5 ≈ Windows default; stamped into
    /// the cursor section because session 0 cannot query it.
    sdr_white_scale: f32,
    /// The scale has been logged once; later lines only on change.
    sdr_white_logged: bool,
    width: u32,
    height: u32,
    /// Handshake advertised `VIDEO_CAP_HDR` (not merely 10-bit). Pins composition
    /// so an SDR client never gets in-band PQ.
    want_hdr: bool,
    /// 10-bit SDR: composed in SDR wide colour where Windows allows it (`want_wcg`), else the
    /// driver's encoder widens BGRA itself. `want_hdr` stays false either way.
    ten_bit_sdr: bool,
    /// Wide colour took at open, so the session keeps it pinned on; off pins it off.
    want_wcg: bool,
    /// Live `advanced_color_enabled`. A change re-opens the driver's encoder.
    display_hdr: bool,
    /// Live SDR wide colour: the driver reads FP16 for an SDR stream. A change re-opens the
    /// driver's encoder.
    display_wcg: bool,
    /// One-shot: the display refused the negotiated depth (poller is ~4 Hz).
    hdr_pin_warned: bool,
    /// Failed pin attempts. Past [`Self::HDR_PIN_EAGER`] retry every
    /// [`Self::HDR_PIN_RETRY_EVERY`]th sample — CCD write+query takes the session-global lock.
    hdr_pin_failures: u32,
    /// Full-chroma 4:4:4.
    want_444: bool,
    /// Wavelet session (`design/pyrowave-windows-host-zerocopy.md`).
    pyrowave: bool,
    /// Off-thread CCD snapshot; the capture loop never runs those queries inline.
    desc_poller: DescriptorPoller,
    /// Last consumed poller sequence (0 = none yet).
    desc_seq: u64,
    /// Two-strikes debounce: act only when a second consecutive sample agrees,
    /// so a topology re-probe blip never re-opens the encoder.
    pending_desc: Option<DisplayDescriptor>,
    /// The topology generation `pending_desc` was sampled under ([`poll_display_hdr`]).
    pending_desc_gen: u64,
    /// A presentation restart is in flight; if no frame resumes past the window,
    /// `try_consume` drops the session (recover-or-drop, no DDA).
    recovering_since: Option<Instant>,
    /// Multi-hundred-ms DWM holes during active flow; warns when they turn metronomic.
    stall_watch: StallWatch,
    /// The stalest drain heartbeat (µs) seen since the last fresh frame.
    max_hb_age_us: u64,
    /// Damage witness for [`StallEvidence::cursor_moved_px`] and the recovery
    /// classifier. user32 only, never the display-config lock; the rule itself is
    /// [`crate::windows::cursor_witness`].
    cursor: CursorWitness,
    /// Micro-probe singleton; `None` unless [`diag_dir`] is on. A missing window reads as
    /// never-stalled, so a report never invents a leg.
    probes: Option<Arc<probes::ProbeEngine>>,
    /// DxgKrnl ETW; `None` unless [`diag_dir`] or [`flow_watch`] is on, or the session refused
    /// to start.
    etw: Option<Arc<dxgkrnl_etw::EtwWatch>>,
    /// `PowerRequestDisplayRequired` for this capturer's life: DWM composes nothing
    /// once the console goes dark. It only keeps a lit display on; the monitor create wakes one.
    _display_wake: Option<pf_frame::session_tuning::DisplayWakeRequest>,
    _keepalive: Box<dyn Send>,
}

#[cfg(test)]
mod tests {
    /// The `CcdTargetKey` packing must equal `pf_frame::dxgi::pack_luid` — the capture target's
    /// `adapter_luid` (packed by pf-frame) is what pf-capture builds its CCD keys from, so a
    /// divergence would make every display-global helper miss its own target's paths. This crate
    /// is the lowest one that depends on both, which is why the assertion lives here.
    #[test]
    fn ccd_key_packing_matches_pf_frame_pack_luid() {
        for (low, high) in [
            (0u32, 0i32),
            (0xdead_beef, -2),
            (7, 0x7fff_ffff),
            (u32::MAX, -1),
        ] {
            let luid = windows::Win32::Foundation::LUID {
                LowPart: low,
                HighPart: high,
            };
            assert_eq!(
                pf_win_display::win_display::CcdTargetKey::from_luid_parts(low, high, 1)
                    .adapter_luid,
                pf_frame::dxgi::pack_luid(luid),
                "packing diverged for LUID {high:#x}:{low:#x}"
            );
        }
    }
}
