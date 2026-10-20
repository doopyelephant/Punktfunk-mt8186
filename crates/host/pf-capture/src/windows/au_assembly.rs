//! The wire chunks and whole access units the driver-encode proxy hands the stream loop,
//! assembled from the slots [`AuReader`] takes. One slot is one [`AuChunk`]; a whole AU
//! concatenates its chunks until LAST, and a FIRST mid-assembly drops the prefix. Each chunk
//! records the progress clocks the supervisor reads and goes to the AU dump when one is open.
//!
//! Pure over the reader, a [`Clocks`] and a wait, so the same code drains the driver's section
//! on Windows and a `Vec` in the tests. The section, its event and the control IOCTLs are the
//! proxy's (`idd_push/driver_encode.rs`).

// Off Windows only the tests read this module.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use crate::au_reader::{AuReader, Taken};
use anyhow::{bail, Context, Result};
use pf_driver_proto::encode::au;
use pf_encode_core::{AuChunk, EncodedFrame};
use pf_frame::health::DriverSplit;
use std::time::{Duration, Instant};

/// The clocks a taken chunk is stamped against: wall time for the wire's `pts_ns`, and the
/// QPC domain the driver's slot stamps are in.
#[derive(Clone, Copy)]
pub(crate) struct Clocks {
    /// Now, in epoch nanoseconds.
    pub now_ns: fn() -> u64,
    /// Age of a QPC stamp in µs; 0 when the stamp is ahead.
    pub qpc_age_us: fn(u64) -> u64,
    /// A span of QPC ticks in µs.
    pub qpc_ticks_us: fn(u64) -> u64,
}

impl Clocks {
    /// A driver `qpc_pts` as the wire's epoch-nanosecond `pts_ns`: now minus the stamp's age.
    /// `0` (no stamp) reads as now.
    fn pts_ns(&self, qpc: u64) -> u64 {
        if qpc == 0 {
            return (self.now_ns)();
        }
        (self.now_ns)().saturating_sub((self.qpc_age_us)(qpc).saturating_mul(1000))
    }

    /// The driver's stamps on one slot as a split. `None` from a driver that wrote none (a zero
    /// submit) or stamps out of order. A present stamp of `0`, or one past the submit (a head
    /// that names the vblank the frame is for), leaves the pool wait unmeasured rather than
    /// negative.
    fn driver_split(&self, t: &Taken) -> Option<DriverSplit> {
        if t.qpc_submit == 0 || t.qpc_published < t.qpc_submit {
            return None;
        }
        let span = |ticks: u64| Duration::from_micros((self.qpc_ticks_us)(ticks));
        Some(DriverSplit {
            pool: (t.qpc_pts != 0 && t.qpc_submit >= t.qpc_pts)
                .then(|| span(t.qpc_submit - t.qpc_pts)),
            encode: span(t.qpc_published - t.qpc_submit),
            ipc: Duration::from_micros((self.qpc_age_us)(t.qpc_published)),
        })
    }
}

/// Access units to disk while `PUNKTFUNK_IDD_DIAG` is on, in the framing the S5 encode probe
/// wrote — `[u32 len][u64 pts_ns][u8 keyframe][bytes]` per chunk — so a field failure replays
/// through the same corpus tooling. A write error drops the dump; the stream never fails for it.
pub(crate) struct AuDump(std::io::BufWriter<std::fs::File>);

impl AuDump {
    /// One file in `dir` per encoder open, named by pid, target and open count, so neither two
    /// sessions nor a re-open share one.
    pub(crate) fn create(dir: &std::path::Path, target_id: u32) -> Option<Self> {
        static OPENS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let open = OPENS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!(
            "pfvd-au-{}-{target_id}-{open}.bin",
            std::process::id()
        ));
        match std::fs::File::create(&path) {
            Ok(f) => {
                tracing::info!(path = %path.display(), "driver encode: dumping access units");
                Some(Self(std::io::BufWriter::new(f)))
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "driver encode: no AU dump");
                None
            }
        }
    }

    /// Append one chunk; `false` means the caller drops the dump.
    fn write(&mut self, c: &AuChunk) -> bool {
        use std::io::Write;
        let r = self
            .0
            .write_all(&(c.data.len() as u32).to_le_bytes())
            .and_then(|()| self.0.write_all(&c.pts_ns.to_le_bytes()))
            .and_then(|()| self.0.write_all(&[u8::from(c.keyframe)]))
            .and_then(|()| self.0.write_all(&c.data));
        if let Err(e) = r {
            tracing::warn!(error = %e, "driver encode: AU dump write failed — dump disabled");
            return false;
        }
        true
    }
}

/// The proxy's read side: the reader, the progress clocks of the last chunk it took, and the
/// dump.
pub(crate) struct AuAssembly<'a> {
    pub(crate) reader: AuReader<'a>,
    clocks: Clocks,
    /// The last taken chunk's `wire_seq` / `source_seq`: AU progress, the supervisor's second
    /// ground-truth clock (`progress`).
    pub(crate) last_wire_seq: u32,
    pub(crate) last_source_seq: u32,
    /// The access unit being taken re-encodes its predecessor's source frame.
    pub(crate) au_repeat: bool,
    /// Age of the last taken chunk's `qpc_pts` when the host took it — present→arrival.
    pub(crate) last_arrival: Option<Duration>,
    /// The driver's own split of that span, when its slot carried the stamps.
    pub(crate) last_split: Option<DriverSplit>,
    /// Bitstream capture while `PUNKTFUNK_IDD_DIAG` is on; dropped on the first write error.
    dump: Option<AuDump>,
}

impl<'a> AuAssembly<'a> {
    /// A slice never takes this long; past it the access unit is truncated and the loop's
    /// stall path resets the encoder.
    const CHUNK_WAIT: Duration = Duration::from_millis(500);

    pub(crate) fn new(reader: AuReader<'a>, clocks: Clocks, dump: Option<AuDump>) -> Self {
        Self {
            reader,
            clocks,
            last_wire_seq: 0,
            last_source_seq: 0,
            au_repeat: false,
            last_arrival: None,
            last_split: None,
            dump,
        }
    }

    /// One taken slot as a wire chunk, recording the progress clocks on the way through:
    /// the sequence pair the supervisor reads as encoder progress, and how old the OS present
    /// stamp already is — the ground truth for "late, not missing". The diagnostic dump takes
    /// its copy here, so a chunked session writes the same bytes in the same order.
    fn chunk(&mut self, t: Taken) -> AuChunk {
        self.last_wire_seq = t.wire_seq;
        self.au_repeat = t.repeat;
        self.last_source_seq = t.source_seq;
        self.last_arrival =
            (t.qpc_pts != 0).then(|| Duration::from_micros((self.clocks.qpc_age_us)(t.qpc_pts)));
        self.last_split = self.clocks.driver_split(&t);
        let chunk = AuChunk {
            data: t.data,
            pts_ns: self.clocks.pts_ns(t.qpc_pts),
            keyframe: t.flags & au::AU_KEYFRAME != 0,
            recovery_anchor: t.flags & au::AU_RECOVERY_ANCHOR != 0,
            recovery_point: t.flags & au::AU_RECOVERY_POINT != 0,
            recovery_close: t.flags & au::AU_RECOVERY_CLOSE != 0,
            chunk_aligned: t.flags & au::AU_CHUNK_ALIGNED != 0,
            first: t.flags & au::AU_FIRST != 0,
            last: t.flags & au::AU_LAST != 0,
        };
        if self.dump.as_mut().is_some_and(|d| !d.write(&chunk)) {
            self.dump = None;
        }
        chunk
    }

    /// The next wire chunk. Non-blocking between access units; inside one, the next chunk is
    /// owed and `wait(ms)` is called until it lands or [`Self::CHUNK_WAIT`] runs out.
    pub(crate) fn poll_chunk(&mut self, wait: &mut dyn FnMut(u32)) -> Result<Option<AuChunk>> {
        let deadline = self
            .reader
            .mid_au()
            .then(|| Instant::now() + Self::CHUNK_WAIT);
        loop {
            if let Some(t) = self.reader.take_next()? {
                return Ok(Some(self.chunk(t)));
            }
            let Some(deadline) = deadline else {
                return Ok(None);
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!(
                    "driver encode: no further chunk of wire frame {} within {:?}",
                    self.reader.next_wire_seq(),
                    Self::CHUNK_WAIT
                );
            }
            wait(left.as_millis().clamp(1, 16) as u32);
        }
    }

    /// The next whole access unit: chunks concatenate in order until LAST closes it. A FIRST
    /// mid-assembly means the driver dropped this AU's tail and moved on: the prefix is
    /// discarded and the next AU starts over, as the loop's `poll_chunked` does for the
    /// streamed wire.
    pub(crate) fn poll(&mut self, wait: &mut dyn FnMut(u32)) -> Result<Option<EncodedFrame>> {
        let start = |c: AuChunk| EncodedFrame {
            data: c.data,
            pts_ns: c.pts_ns,
            keyframe: c.keyframe,
            recovery_anchor: c.recovery_anchor,
            recovery_point: c.recovery_point,
            recovery_close: c.recovery_close,
            chunk_aligned: c.chunk_aligned,
        };
        let Some(first) = self.poll_chunk(wait)? else {
            return Ok(None);
        };
        let mut last = first.last;
        let mut frame = start(first);
        while !last {
            let c = self
                .poll_chunk(wait)?
                .context("driver encode: access unit ended without its LAST chunk")?;
            last = c.last;
            if c.first {
                frame = start(c);
            } else {
                frame.data.extend_from_slice(&c.data);
                frame.keyframe |= c.keyframe;
            }
        }
        Ok(Some(frame))
    }

    /// The `wire_seq` a `RESET` restarts the domain at: where this reader stands, so the
    /// loop's `au_seq` keeps matching. A half-read AU already spent its index on the wire: the
    /// loop closes that frame at the next FIRST, so the domain restarts past it.
    pub(crate) fn reset_base(&self) -> u32 {
        self.reader
            .next_wire_seq()
            .wrapping_add(u32::from(self.reader.mid_au()))
    }
}
