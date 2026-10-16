//! Live capture on Linux: [`PortalCapturer`] (xdg ScreenCast portal or a virtual output's
//! node, through PipeWire) and the direct `ext-image-copy-capture-v1` [`WlCapturer`], plus the
//! failure signals and the frame mailbox they share.
//!
//! PipeWire frames leave through a queue that waits out each producer render
//! (`pipewire::queue`) plus a wakeup edge; the direct capturer's are
//! rendered on arrival and leave through a one-deep [`FrameSlot`]. Payload may
//! be packed RGB, NV12, YUV444, 10-bit PQ, or a dmabuf that never touches the
//! CPU. Size is the negotiated PipeWire format, not the portal hint.

use super::{CapturedFrame, PixelFormat, ZeroCopyPolicy};

// Gamescope's PipeWire node has no `SPA_META_Cursor`; this fills `cursor_live` from XFixes.
mod xfixes_cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;

/// One-deep overwriting mailbox: producer drops oldest. `sync_channel` is
/// drop-newest (`try_send` discards the fresh frame once full). A queued
/// [`CapturedFrame`] can own a dup'd dmabuf or CUDA buffer, so depth > 1
/// pins compositor buffers.
type FrameSlot = Arc<std::sync::Mutex<Option<CapturedFrame>>>;

/// `wait_arrival` for the direct capturer: returns once `slot` holds a frame,
/// `deadline` passes, or the producer is broken or gone. Never consumes: the
/// frame stays for `try_latest`, which also classifies a dead producer.
fn wait_for_frame(
    slot: &FrameSlot,
    wake: &Receiver<()>,
    broken: &AtomicBool,
    deadline: std::time::Instant,
) {
    if broken.load(Ordering::Relaxed) {
        return;
    }
    loop {
        if slot.lock().is_ok_and(|s| s.is_some()) {
            return;
        }
        let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return;
        };
        if wake.recv_timeout(left).is_err() {
            return;
        }
    }
}

/// Drains stale wakeup edges so the next [`wait_for_frame`] cannot return
/// early. `true` when the producer thread is gone (its sender dropped).
fn drain_edges(wake: &Receiver<()>) -> bool {
    loop {
        match wake.try_recv() {
            Ok(()) => continue,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
}

#[derive(Clone)]
struct CaptureSignals {
    /// Failure memory scoped to this producer identity and shared with encoder frames.
    health: pf_zerocopy::ZeroCopyHealth,
    /// Per-frame de-pad runs only while set; pooling a 5K capturer is cheap
    /// between streams.
    active: Arc<AtomicBool>,
    /// Format agreed. Timeout diagnosis: mismatch vs idle/unmapped compositor.
    negotiated: Arc<AtomicBool>,
    /// Stream is `Streaming`. Distinguishes a static desktop from a dead source.
    streaming: Arc<AtomicBool>,
    /// This stream drives the graph: the producer paints only in cycles the
    /// loop thread's pacer starts on its requests. Cleared with `streaming`.
    driving: Arc<AtomicBool>,
    /// GPU import is gone for this stream (worker death, or a tiled or planar
    /// frame the CPU de-pad cannot read). Never cleared.
    broken: Arc<AtomicBool>,
    /// The stream reached `Error` (e.g. "no more input formats"). Terminal: it never delivers.
    errored: Arc<AtomicBool>,
    /// Buffers the producer sent again while this capture still held them (PipeWire < 1.6,
    /// no `node.reliable`). Each is re-held under a new generation; the count rides the
    /// provenance line, and [`Capturer::take_reference_risk`] turns a step into one IDR.
    resent: Arc<std::sync::atomic::AtomicU64>,
    hdr_negotiated: Arc<AtomicBool>,
    /// Thread actually advertised the EGL→CUDA dmabuf-only offer. `plan.build_importer`
    /// is not enough: a failed importer means no dmabuf was offered, so a
    /// timeout must not latch the GPU offer off.
    gpu_dmabuf_offer: Arc<AtomicBool>,
    /// Overlay from every buffer's `SPA_META_Cursor`, including cursor-only
    /// buffers that never become frames. Gamescope XFixes publishes here too.
    cursor_live: Arc<std::sync::Mutex<Option<pf_frame::CursorOverlay>>>,
    /// The host forwards or blends [`Self::cursor_live`] itself, so the CPU copy must not
    /// bake the pointer into its pixels as well. Set by [`Capturer::set_cursor_forward`].
    host_places_cursor: Arc<AtomicBool>,
    /// Packed `(w << 32) | h`; `0` until `param_changed`. Gamescope cursor
    /// maps root-space into frame space (`-w/-h` vs `-W/-H` are independent).
    frame_size: Arc<std::sync::atomic::AtomicU64>,
    /// NVIDIA zero-copy importer, usually the isolated worker. The loop thread
    /// fills it after negotiation; the consumer imports held frames through it
    /// ([`PortalCapturer::import_held`]) and retires it on a LINEAR failure.
    importer: Arc<std::sync::Mutex<Option<pf_zerocopy::Importer>>>,
    /// `importer` is `Some`. Read per frame on the loop thread without the lock.
    has_importer: Arc<AtomicBool>,
    /// `importer` runs in this process, its GL context current on the loop thread only. Read per
    /// frame without the lock, which the consumer holds across a whole worker import.
    importer_in_process: Arc<AtomicBool>,
}

/// The NVENC raw lane may take this capture: the encoder offers it and the fused convert samples
/// at least one tiled BGRx layout. With none, a tiled producer could only be offered LINEAR.
fn raw_lane_ok(policy: &ZeroCopyPolicy) -> bool {
    policy.nvenc_raw_dmabuf
        && !pf_zerocopy::fused_convert_modifiers(pf_zerocopy::drm::XR24).is_empty()
}

/// `importable` narrowed to the modifiers the raw lane's fused convert samples, order kept.
/// LINEAR stays.
fn raw_lane_modifiers(importable: &[u64], fused: &[u64]) -> Vec<u64> {
    importable
        .iter()
        .copied()
        .filter(|m| *m == 0 || fused.contains(m))
        .collect()
}

/// Producer identity plus the consumer policy whose failures must stay independent.
fn health_identity(base: u64, policy: &ZeroCopyPolicy) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    base.hash(&mut hash);
    policy.backend_is_vaapi.hash(&mut hash);
    policy.pyrowave_session.hash(&mut hash);
    policy.nvenc_raw_dmabuf.hash(&mut hash);
    policy.encoder_modifiers.hash(&mut hash);
    hash.finish()
}

impl CaptureSignals {
    fn new(health: pf_zerocopy::ZeroCopyHealth) -> Self {
        Self {
            health,
            active: Arc::new(AtomicBool::new(false)),
            negotiated: Arc::new(AtomicBool::new(false)),
            streaming: Arc::new(AtomicBool::new(false)),
            driving: Arc::new(AtomicBool::new(false)),
            broken: Arc::new(AtomicBool::new(false)),
            errored: Arc::new(AtomicBool::new(false)),
            resent: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            hdr_negotiated: Arc::new(AtomicBool::new(false)),
            gpu_dmabuf_offer: Arc::new(AtomicBool::new(false)),
            cursor_live: Arc::new(std::sync::Mutex::new(None)),
            host_places_cursor: Arc::new(AtomicBool::new(false)),
            frame_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            importer: Arc::new(std::sync::Mutex::new(None)),
            has_importer: Arc::new(AtomicBool::new(false)),
            importer_in_process: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A dmabuf frame this capture publishes. The hold is not optional: capture never publishes
    /// a buffer the producer may rewrite while the encoder reads it.
    fn held_dmabuf(
        &self,
        fd: std::os::fd::OwnedFd,
        fourcc: u32,
        modifier: u64,
        (offset, stride): (u32, u32),
        plane1: Option<(u32, u32)>,
        hold: pf_frame::FrameHold,
    ) -> pf_frame::DmabufFrame {
        pf_frame::DmabufFrame {
            fd,
            fourcc,
            modifier,
            plane1,
            offset,
            stride,
            hold: Some(hold),
            health: self.health.clone(),
            rebuild: self.broken.clone(),
        }
    }
}

// ScreenCast/RemoteDesktop handshake + GNOME colour-mode probe. Async,
// not per-frame. `gnome_hdr_monitor_active` is re-exported from `lib.rs`.
mod portal;
pub use portal::gnome_hdr_monitor_active;

// PipeWire consumer (`!Send`, owns its thread) and its front end, [`PortalCapturer`].
mod pipewire;
pub(crate) use pipewire::PortalCapturer;
// Client-allocated dmabufs for the direct capture path.
mod gbm_pool;
// Direct `ext-image-copy-capture-v1` capture, with no portal and no PipeWire.
mod wl_capture;
pub(crate) use wl_capture::{hdr_capture_globals, output_is_hdr10, WlCapturer};
// Negotiation POD builders and cursor-meta parser + CPU blits. Pure enough
// to unit-test without a compositor.
mod pw_cursor;
mod pw_pods;
// Explicit sync (`SPA_META_SyncTimeline`): syncobj waits and release signals.
mod sync_timeline;

#[cfg(test)]
mod tests {
    use super::raw_lane_modifiers;

    /// NVIDIA 595, 2560×1440 BGRx: EGL imports the compressed block-linear layout KWin's gbm
    /// picks; the fused convert's Vulkan device samples only the plain ones.
    #[test]
    fn raw_lane_offer_drops_what_the_convert_cannot_sample() {
        const PLAIN: u64 = 0x0300_0000_0060_6010;
        const COMPRESSED: u64 = 0x0300_0000_00e0_8014;
        let egl = [PLAIN, COMPRESSED, PLAIN + 1, 0];
        let fused = [PLAIN + 1, PLAIN];
        assert_eq!(raw_lane_modifiers(&egl, &fused), vec![PLAIN, PLAIN + 1, 0]);
        assert_eq!(
            raw_lane_modifiers(&egl, &[]),
            vec![0],
            "LINEAR always stays"
        );
        assert!(raw_lane_modifiers(&[], &fused).is_empty());
    }
}
