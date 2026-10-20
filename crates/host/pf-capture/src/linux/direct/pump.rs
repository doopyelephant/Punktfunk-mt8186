//! The direct capture's frame loop, on the capture thread.

use super::protocol::CaptureFrame;
use super::session::{drain, wait_readable};
use super::Opened;
use crate::linux::{FrameSlot, StreamShared};
use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::{SystemTime, UNIX_EPOCH};

/// How long a capture may go without a `ready` before the stream is called dead. Long
/// enough that an idle desktop (which legitimately delivers nothing) never trips it; the
/// stream loop's own liveness check is what notices a genuinely stalled source.
const POLL_SLICE: std::time::Duration = std::time::Duration::from_millis(100);

/// The frame loop: arm a capture whenever none is outstanding and a buffer is free, and hand
/// each ready one to `slot`. Ends on `quit` or a stopped session; a failed frame, a resize
/// or a re-lit head ends it with an error, and the capture rebuilds.
pub(super) fn pump(
    mut o: Opened,
    slot: &FrameSlot,
    wake: &SyncSender<()>,
    shared: &StreamShared,
    quit: &AtomicBool,
    header_pts: bool,
) -> Result<()> {
    let (w, h) = o.size;
    let mut frame: Option<(CaptureFrame, usize)> = None;
    let mut delivered: u64 = 0;
    // Clocks drift by microseconds over a long session; re-paired on the same cadence as
    // the portal path's provenance window.
    let mut rt_minus_mono_ns = crate::linux::pipewire::realtime_minus_monotonic_ns();
    let mut repaired = std::time::Instant::now();
    while !quit.load(Ordering::Relaxed) && !o.st.stopped {
        // Arm whenever nothing is outstanding and a buffer is free. A returned buffer
        // wakes the poll below, so this runs the moment the encoder lets go.
        if frame.is_none() {
            if let Some(idx) = o.free.take() {
                let f = o.session.create_frame(&o.qh, ());
                f.attach_buffer(&o.buffers[idx]);
                f.damage_buffer(0, 0, w as i32, h as i32);
                f.capture();
                o.st.ready = false;
                o.st.failed = None;
                o.st.presented_ns = None;
                frame = Some((f, idx));
            }
        }
        if repaired.elapsed() >= std::time::Duration::from_secs(30) {
            rt_minus_mono_ns = crate::linux::pipewire::realtime_minus_monotonic_ns();
            repaired = std::time::Instant::now();
        }
        o.conn.flush().context("wayland flush")?;
        wait_readable(&o.conn, &o.wake_r, POLL_SLICE)?;
        drain(&o.wake_r);
        o.queue.dispatch_pending(&mut o.st).context("dispatch")?;
        if let Some(reason) = o.st.failed.take() {
            if let Some((f, idx)) = frame.take() {
                f.destroy();
                o.free.put(idx);
            }
            bail!("compositor failed the capture frame (reason {reason})");
        }
        if o.st.resized {
            bail!("capture source changed size — rebuilding the capture");
        }
        if o.st.color_changed {
            bail!("output colour description changed — rebuilding the capture");
        }
        if !o.st.ready {
            continue;
        }
        o.st.ready = false;
        let Some((f, idx)) = frame.take() else {
            continue;
        };
        f.destroy();
        // The compositor stamps `presentation_time` on CLOCK_MONOTONIC; the wire
        // speaks realtime-since-epoch. `wire_pts` rebases it and falls back to the
        // delivery stamp when the result is implausible, exactly as the portal path
        // does for `SPA_META_Header`. Stamped at `ready`: the fence wait and the
        // import in `deliver` are not part of the frame's age.
        let delivery_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let presented_ns = o.st.presented_ns.take().filter(|_| header_pts);
        let pts_ns = crate::pts_provenance::wire_pts(
            presented_ns.map(|p| p as i64),
            delivery_ns,
            rt_minus_mono_ns,
        )
        .pts_ns;
        let captured = o.deliver(idx, pts_ns, shared)?;
        if let Ok(mut s) = slot.lock() {
            *s = Some(captured);
        }
        let _ = wake.try_send(());
        delivered += 1;
        if delivered == 1 {
            tracing::info!("direct wayland capture: first frame delivered");
        }
    }
    if let Some((f, idx)) = frame.take() {
        f.destroy();
        o.free.put(idx);
    }
    o.session.destroy();
    o.source.destroy();
    Ok(())
}
