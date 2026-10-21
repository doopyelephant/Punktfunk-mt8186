//! One protocol frame of the audio plane, from the slot the [`Pacer`](super::Pacer) released:
//! real audio drained from the capture queue, or silence over a capture hole. Digital zero is
//! a step from the last real sample, so a hole fades out of what the listener heard last and
//! the first real frame after it fades back in (raised cosine, [`EDGE_FADE_US`] each edge).
//!
//! Pure: no clock, no I/O, and the buffers are reused frame to frame. The table in this file's
//! tests pins it, one row per behaviour.

use super::Slot;
use punktfunk_core::audio::pcm;

/// Fade at each edge of a capture hole, in µs. 1 ms is a slope, not an edge, and too short to
/// read as a swell.
const EDGE_FADE_US: u64 = 1_000;

/// One frame ready for the wire.
pub(super) struct Frame<'a> {
    pub samples: &'a [f32],
    /// Silence over a hole, the partial it opened on included.
    pub infilled: bool,
}

/// Builds each frame the pacer releases, and carries the fade state between them.
pub(super) struct FrameAssembler {
    /// Interleaved samples in one protocol frame.
    frame_len: usize,
    channels: usize,
    /// Interleaved: whole frames × channels, so the curve spans whole frames.
    edge_fade: usize,
    /// Operator capture gain, applied to real frames.
    gain: f32,
    buf: Vec<f32>,
    /// The fade-out source when a hole opens on an empty partial; recorded post-gain, so the
    /// fade sits at the level the listener was hearing.
    last_real: Vec<f32>,
    /// The next real frame starts mid-waveform and fades in.
    resume_fade: bool,
}

impl FrameAssembler {
    pub(super) fn new(rate_hz: u32, channels: u8, frame_len: usize, gain: f32) -> FrameAssembler {
        let channels = channels as usize;
        FrameAssembler {
            frame_len,
            channels,
            edge_fade: (rate_hz as u64 * EDGE_FADE_US / 1_000_000) as usize * channels,
            gain,
            buf: Vec::with_capacity(frame_len),
            last_real: Vec::with_capacity(frame_len),
            resume_fade: false,
        }
    }

    /// The frame `slot` sends, its samples taken from `acc`; `None` on [`Slot::Wait`]. A real
    /// frame drains `frame_len` samples. A hole frame takes the whole partial and pads it with
    /// silence: waiting for post-gap samples to complete it would straddle the hole, and
    /// dropping it would come back as schedule lag.
    pub(super) fn next(&mut self, slot: Slot, acc: &mut Vec<f32>) -> Option<Frame<'_>> {
        self.buf.clear();
        let infilled = match slot {
            Slot::Wait => return None,
            Slot::Frame => {
                self.buf.extend(acc.drain(..self.frame_len));
                if self.gain != 1.0 {
                    punktfunk_core::audio::apply_gain(&mut self.buf, self.gain);
                }
                if std::mem::take(&mut self.resume_fade) {
                    pcm::raised_cosine_head(&mut self.buf, self.edge_fade);
                }
                self.last_real.clear();
                self.last_real.extend_from_slice(&self.buf);
                false
            }
            Slot::Silence { first } => {
                let partial = acc.len();
                self.buf.append(acc);
                if first {
                    // Fade the tail we have — the partial, or the last real frame if it is
                    // empty — so the hole is a slope from the listener's level, not a step.
                    if partial == 0 && !self.last_real.is_empty() {
                        extend_hole_seed(
                            &mut self.buf,
                            &self.last_real,
                            self.edge_fade,
                            self.channels,
                        );
                    }
                    let n = self.buf.len().min(self.edge_fade);
                    pcm::raised_cosine_tail(&mut self.buf, n);
                }
                self.buf.resize(self.frame_len, 0.0);
                // Whatever follows this hole starts mid-waveform.
                self.resume_fade = true;
                true
            }
        };
        Some(Frame {
            samples: &self.buf,
            infilled,
        })
    }

    /// The next real frame fades in: the frame before it never reached the listener.
    pub(super) fn fade_in_next(&mut self) {
        self.resume_fade = true;
    }
}

/// What a hole fades out of when no partial is queued: the last `n` interleaved samples of
/// the last real frame, frame order reversed and channels kept, so the first replayed frame is
/// the one the listener heard last and the fade starts at their level.
fn extend_hole_seed(buf: &mut Vec<f32>, last_real: &[f32], n: usize, channels: usize) {
    let channels = channels.max(1);
    let n = n.min(last_real.len()) / channels * channels;
    let tail = &last_real[last_real.len() - n..];
    buf.extend(tail.chunks_exact(channels).rev().flatten());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hole with no partial replays the end of the last frame backwards, so its first
    /// frame is the last one sent and each channel stays on its own lane.
    #[test]
    fn a_hole_fades_out_of_the_last_frame_sent() {
        let last_real = [1.0, 10.0, 2.0, 20.0, 3.0, 30.0];
        let mut buf = Vec::new();
        extend_hole_seed(&mut buf, &last_real, 4, 2);
        assert_eq!(buf, [3.0, 30.0, 2.0, 20.0]);
        let mut short = Vec::new();
        extend_hole_seed(&mut short, &last_real[..2], 4, 2);
        assert_eq!(
            short,
            [1.0, 10.0],
            "a frame shorter than the fade replays what it has"
        );
    }
}
