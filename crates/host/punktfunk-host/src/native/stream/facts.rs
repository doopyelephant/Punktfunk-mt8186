//! The parts of [`StreamState`](super::state::StreamState) that bring-up hands over once and no
//! rebuild replaces: the handles other threads read.

use super::*;
use crate::native::bitrate::EncoderCeiling;

/// What the control task, the send and input threads, the clipboard and `/status` read while the
/// loop writes it, or the other way round.
pub(super) struct Shared {
    /// [`crate::native::wiring::SessionShared::acked`] and `lossy_window`.
    pub(super) acked: Arc<AtomicU64>,
    pub(super) lossy_window: Arc<AtomicBool>,
    /// [`crate::native::wiring::SessionShared::arrival_poke`]: the running capturer's, for
    /// the control task.
    pub(super) arrival_poke: crate::native::wiring::ArrivalPoke,
    /// The Windows driver's pool drops, handed to the send thread's recorder sample.
    pub(super) driver_dropped: Arc<AtomicU64>,
    pub(super) screen_wire: Arc<crate::native::wiring::ScreenWire>,
    pub(super) send_spread_us: Arc<AtomicU32>,
    pub(super) wire_rekeys: Arc<AtomicU32>,
    pub(super) live_mode: Arc<AtomicU64>,
    pub(super) force_idr: Arc<AtomicBool>,
    pub(super) capture_health: Arc<std::sync::Mutex<Option<pf_capture::CaptureHealth>>>,
    /// The live encoder's framing: forwarded cursor positions map through it, and so does
    /// the input thread's absolute input. Written on every encoder open.
    pub(super) frame_map: super::super::input::FrameMap,
    /// The session gamescope's X displays, for the clipboard. Written on every capture attach.
    #[cfg(target_os = "linux")]
    pub(super) gamescope_xwayland: pf_clipboard::GamescopeXwayland,
    pub(super) resize_ms: Arc<AtomicU32>,
    pub(super) live_bitrate: Arc<AtomicU32>,
    pub(super) encoder_ceiling: Arc<std::sync::Mutex<EncoderCeiling>>,
    pub(super) cadence_degraded: Arc<AtomicBool>,
    pub(super) cadence_behind_score: Arc<AtomicU32>,
    pub(super) client_packets_received: Arc<AtomicU32>,
    pub(super) cursor_client_draws: Arc<AtomicBool>,
}
