//! What the two pixel-less capturers share: IDD-push, where the driver encodes, and the
//! capture worker's, where the worker does.
//!
//! Neither hands the stream loop pixels. A delivery is the news that the encoder's pool took
//! a new frame, read off the encoder's own counter, and the loop paces and sizes itself from
//! it. [`DeliveryGate`] decides when there is news; [`frame`] is what a delivery looks like.
//!
//! Pure, so the gate's rule has tests on every target.

// Off Windows only the tests read this module.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use pf_frame::{CapturedFrame, FramePayload, PixelFormat, Provenance};
use std::time::{SystemTime, UNIX_EPOCH};

/// When a pixel-less capturer has a delivery for the loop. `G` is the geometry the loop
/// sizes the encoder from.
pub(crate) struct DeliveryGate<G> {
    /// The pool counter at the last delivery.
    seq: u64,
    /// The geometry last delivered. A change must reach the loop even over a desktop composing
    /// nothing: it rebuilds the encoder off the frame.
    delivered: Option<G>,
}

impl<G: PartialEq> DeliveryGate<G> {
    pub(crate) fn new() -> Self {
        Self {
            seq: 0,
            delivered: None,
        }
    }

    /// Whether this tick delivers. `source_seq` is the encoder's pool counter, the only source
    /// clock this side has, and `None` until the encoder opens: the loop needs a frame to open
    /// it. A re-opened encoder's fresh section reads 0 until the pool takes a frame, which is
    /// no news.
    pub(crate) fn admit(&mut self, source_seq: Option<u64>, geometry: G) -> bool {
        let seq = source_seq.unwrap_or(0);
        if source_seq.is_some()
            && (seq == 0 || seq == self.seq)
            && self.delivered.as_ref() == Some(&geometry)
        {
            return false;
        }
        self.seq = seq;
        self.delivered = Some(geometry);
        true
    }
}

/// The `seq`th delivery of a pixel-less capturer, stamped now. No pixels cross the boundary:
/// the loop sees `Encoder::ready_aus` answer `Some` and owes wire indexes instead of
/// submitting this frame. The encoder stamps each access unit with the frame's own present
/// time, so this side reports the sequence only.
pub(crate) fn frame(seq: u64, width: u32, height: u32, format: PixelFormat) -> CapturedFrame {
    CapturedFrame {
        provenance: Provenance::source(seq, 0),
        width,
        height,
        pts_ns: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
        format,
        payload: FramePayload::Cpu(Vec::new()),
        cursor: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HD: (u32, u32) = (1920, 1080);
    const QHD: (u32, u32) = (2560, 1440);

    /// One gate through a session: the pool counter and geometry of each tick, and whether
    /// the tick delivers.
    #[test]
    fn a_tick_delivers_only_news() {
        let ticks = [
            (None, HD, true, "the loop needs a frame to open the encoder"),
            (None, HD, true, "and needs one until it is open"),
            (Some(0), HD, false, "a fresh section is no news"),
            (Some(1), HD, true, "the pool took a frame"),
            (Some(1), HD, false, "the desktop composed nothing"),
            (Some(2), HD, true, "the pool took another"),
            (Some(2), QHD, true, "a new size over a still desktop"),
            (Some(2), QHD, false, "delivered once"),
            (Some(0), HD, true, "a new size ahead of the re-opened pool"),
            (Some(0), HD, false, "whose fresh section is no news either"),
            (Some(1), HD, true, "until it takes a frame"),
        ];
        let mut gate = DeliveryGate::new();
        for (i, (seq, geometry, delivers, why)) in ticks.into_iter().enumerate() {
            assert_eq!(gate.admit(seq, geometry), delivers, "tick {i}: {why}");
        }
    }

    #[test]
    fn a_delivery_carries_no_pixels() {
        let f = frame(3, 1920, 1080, PixelFormat::Nv12);
        assert!(matches!(&f.payload, FramePayload::Cpu(px) if px.is_empty()));
        assert_eq!(f.provenance, Provenance::source(3, 0));
        assert_eq!(
            (f.width, f.height, f.format),
            (1920, 1080, PixelFormat::Nv12)
        );
        assert!(f.cursor.is_none());
    }
}
