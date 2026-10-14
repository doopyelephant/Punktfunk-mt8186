//! Hung-decoder detection for the MediaCodec decode loop, and what repeated hangs cost the
//! bring-up ladder.

use std::time::{Duration, Instant};

/// How long access units may wait while the codec frees no input slot before the decoder counts
/// as hung. A working codec frees a slot per decoded frame: 1 s is 60 missed slots at 60 Hz.
pub(crate) const INPUT_STALL_PATIENCE: Duration = Duration::from_secs(1);

/// Trips once access units have waited [`INPUT_STALL_PATIENCE`] with no input slot offered.
///
/// A hung hardware decoder raises no error. It stops taking input, so no keyframe can reach it,
/// and the loop's keyframe requests change nothing. A codec that takes no input shows no new
/// picture either, so the screen is already frozen whenever this trips.
#[derive(Default)]
pub(crate) struct InputStall {
    since: Option<Instant>,
}

impl InputStall {
    /// One loop pass. `offered`: the codec freed an input slot this pass. `waiting`: access units
    /// are still parked after feeding.
    pub(crate) fn poll(&mut self, offered: bool, waiting: bool, now: Instant) -> bool {
        if offered || !waiting {
            self.since = None;
            return false;
        }
        now.duration_since(*self.since.get_or_insert(now)) >= INPUT_STALL_PATIENCE
    }
}

/// How long the decoder may owe output before the loop asks for a re-anchor keyframe. A
/// silence window, not the gate's per-AU streak: MediaCodec does not pair inputs with outputs.
/// Well past any decoder's first-frame latency.
pub(crate) const NO_OUTPUT_PATIENCE: Duration = Duration::from_millis(500);

/// Trips once the decoder has owed output for [`NO_OUTPUT_PATIENCE`]: it never got the opening
/// IDR, or its reference chain is gone. A hardware decoder then emits nothing, and under
/// infinite GOP nothing re-anchors it unless the client asks.
pub(crate) struct NoOutput {
    /// The latest pass that produced a frame or owed none.
    owed_since: Instant,
    handled_at_output: u64,
}

impl NoOutput {
    pub(crate) fn new(now: Instant) -> NoOutput {
        NoOutput {
            owed_since: now,
            handled_at_output: 0,
        }
    }

    /// One pass. `handled`: AUs fed to the codec or withheld from it. A withheld AU owes output
    /// too, so a stretch whose anchor was lost still trips. At most once per window.
    pub(crate) fn poll(&mut self, handled: u64, had_output: bool, now: Instant) -> bool {
        // Owing nothing restarts the window, so the first AU after a still stretch gets the full
        // patience instead of tripping on the pause before it.
        if had_output || handled == self.handled_at_output {
            self.owed_since = now;
            self.handled_at_output = handled;
            return false;
        }
        if now.duration_since(self.owed_since) < NO_OUTPUT_PATIENCE {
            return false;
        }
        log::warn!(
            "decode: no output for {} ms with {} AU(s) fed or withheld — requesting a re-anchor keyframe",
            now.duration_since(self.owed_since).as_millis(),
            handled - self.handled_at_output
        );
        self.owed_since = now;
        self.handled_at_output = handled;
        true
    }
}

/// Frames a decoder shows before its rung counts as working on this device. Five seconds
/// at 60 Hz: a configuration that stops its decoder does so within a few dozen frames,
/// the pool's worth; one that ran five seconds works.
pub(crate) const SETTLED_FRAMES: u64 = 300;

/// The rung a decoder's replacement starts at once this one started and then failed, over
/// the ladder's present backends (`None` = no reader). A started decoder proved its reader's
/// allocation, so a hang on a reader rung sheds the reader itself; a reader-free rung sheds
/// its keys. `reader`: the failed decoder rendered into a reader. `None` at the ladder's end.
pub(crate) fn rung_after_failure(
    backends: &[Option<bool>],
    rung: usize,
    reader: bool,
) -> Option<usize> {
    (rung + 1..backends.len()).find(|&i| !reader || backends[i].is_none())
}

/// Clean, unsettled failures in a row before the ladder moves.
const UNSETTLED_PER_RUNG: u32 = 2;

/// The decoder bring-up ladder's first rung. A decoder can start cleanly and then hang a few
/// frames in, every time: nothing was refused, so the ladder alone never moves past it.
///
/// The floor moves only on evidence a working decoder cannot produce: two decoders in a row,
/// on a rung where none has settled, each failing on an unbroken stream. A rung that once
/// showed [`SETTLED_FRAMES`] keeps its place whatever fails later, and a failure after a loss
/// counts for nothing: loss hangs working decoders too.
#[derive(Default)]
pub(crate) struct RungFloor {
    rung: usize,
    unsettled: u32,
    /// A decoder on this rung settled.
    proven: bool,
}

impl RungFloor {
    pub(crate) fn rung(&self) -> usize {
        self.rung
    }

    /// A decoder failed after showing `rendered` frames. `clean`: it was fed nothing past a
    /// loss and reported no error. `lower`: the rung below the one it ran on, if the ladder has
    /// one. `true`: the floor moved there.
    pub(crate) fn note_failed(&mut self, rendered: u64, clean: bool, lower: Option<usize>) -> bool {
        self.proven |= rendered >= SETTLED_FRAMES;
        if self.proven || !clean {
            self.unsettled = 0;
            return false;
        }
        self.unsettled += 1;
        match lower {
            Some(lower) if self.unsettled >= UNSETTLED_PER_RUNG => {
                *self = RungFloor {
                    rung: lower,
                    ..RungFloor::default()
                };
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        rung_after_failure, InputStall, NoOutput, RungFloor, INPUT_STALL_PATIENCE,
        NO_OUTPUT_PATIENCE, SETTLED_FRAMES,
    };
    use std::time::{Duration, Instant};

    /// The full ladder: overlay reader, GPU-composited reader, then three reader-free rungs.
    /// A hang skips the second reader; the overlay bit only matters at start.
    #[test]
    fn a_hung_decoder_sheds_the_reader_then_the_keys() {
        let backends = [Some(true), Some(false), None, None, None];
        assert_eq!(rung_after_failure(&backends, 0, true), Some(2));
        assert_eq!(rung_after_failure(&backends, 1, true), Some(2));
        // The reader failed to create, so the decoder already ran on the SurfaceView.
        assert_eq!(rung_after_failure(&backends, 1, false), Some(2));
        assert_eq!(rung_after_failure(&backends, 2, false), Some(3));
        assert_eq!(rung_after_failure(&backends, 4, false), None);
        // No reader wanted at all: plain descent.
        assert_eq!(rung_after_failure(&[None, None, None], 0, false), Some(1));
        assert_eq!(rung_after_failure(&[None], 0, false), None);
    }

    /// A decoder that starts and then hangs on an unbroken stream, every time, walks the
    /// ladder, and each rung gets its own two tries.
    #[test]
    fn two_clean_unsettled_failures_in_a_row_move_the_floor() {
        let mut f = RungFloor::default();
        assert!(!f.note_failed(10, true, Some(1)));
        assert!(f.note_failed(9, true, Some(1)));
        assert_eq!(f.rung(), 1);
        assert!(!f.note_failed(0, true, Some(2)));
        assert!(f.note_failed(0, true, Some(2)));
        assert_eq!(f.rung(), 2);
        // The last rung has nowhere to go.
        assert!(!f.note_failed(0, true, None));
        assert!(!f.note_failed(0, true, None));
        assert_eq!(f.rung(), 2);
    }

    /// Loss hangs working decoders. A failure after one counts for nothing and breaks the row.
    #[test]
    fn a_failure_after_a_loss_never_moves_the_floor() {
        let mut f = RungFloor::default();
        for _ in 0..8 {
            assert!(!f.note_failed(5, false, Some(1)));
        }
        assert!(!f.note_failed(5, true, Some(1)));
        assert!(!f.note_failed(5, false, Some(1)));
        assert!(!f.note_failed(5, true, Some(1)));
        assert_eq!(f.rung(), 0);
    }

    /// A rung that showed a second of picture works on this device, whatever fails later.
    #[test]
    fn a_rung_that_settled_keeps_its_place() {
        let mut f = RungFloor::default();
        assert!(!f.note_failed(SETTLED_FRAMES, true, Some(1)));
        for _ in 0..8 {
            assert!(!f.note_failed(3, true, Some(1)));
        }
        assert_eq!(f.rung(), 0);
        // A lower rung has to prove itself again.
        let mut f = RungFloor::default();
        assert!(!f.note_failed(3, true, Some(1)));
        assert!(f.note_failed(3, true, Some(1)));
        assert!(!f.note_failed(3, true, Some(2)));
        assert!(f.note_failed(3, true, Some(2)));
        assert_eq!(f.rung(), 2);
    }

    /// A still host sends one frame after seconds of nothing. That frame must reach the screen:
    /// tripping on it arms the freeze and holds the very picture that carries the change.
    #[test]
    fn an_au_after_a_still_host_gets_the_full_patience() {
        let t0 = Instant::now();
        let mut b = NoOutput::new(t0);
        assert!(!b.poll(1, true, t0));
        assert!(!b.poll(1, false, t0 + Duration::from_secs(2)));
        let fed_at = t0 + Duration::from_millis(2005);
        assert!(!b.poll(2, false, fed_at));
        assert!(!b.poll(2, true, fed_at + Duration::from_millis(8)));
        // A decoder that really goes silent still trips, once, after the patience.
        let silent = fed_at + Duration::from_millis(20);
        assert!(!b.poll(3, false, silent));
        assert!(b.poll(4, false, silent + NO_OUTPUT_PATIENCE));
        assert!(!b.poll(
            5,
            false,
            silent + NO_OUTPUT_PATIENCE + Duration::from_millis(5)
        ));
    }

    /// After a loss the codec may get nothing: every AU is withheld until the anchor. If the
    /// anchor is lost the window must still trip, which only counting withheld AUs does.
    #[test]
    fn withheld_aus_still_trip_the_window() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let fed = 10;
        let mut counted = NoOutput::new(t0);
        let mut fed_only = NoOutput::new(t0);
        assert!(!counted.poll(fed, true, t0));
        assert!(!fed_only.poll(fed, true, t0));
        let (mut trips, mut fed_only_trips) = (0, 0);
        for withheld in 1..=40u64 {
            let now = t0 + ms(16 * withheld);
            trips += u32::from(counted.poll(fed + withheld, false, now));
            fed_only_trips += u32::from(fed_only.poll(fed, false, now));
        }
        assert_eq!(
            trips, 1,
            "640 ms of withheld AUs trips the 500 ms window once"
        );
        assert_eq!(fed_only_trips, 0);
    }

    #[test]
    fn only_a_codec_that_stops_taking_input_trips() {
        let mut s = InputStall::default();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        // A working codec frees a slot each pass: never trips, however long the backlog.
        for i in 0..300 {
            assert!(!s.poll(true, true, t0 + ms(i * 10)));
        }
        // A still host parks nothing, so an idle stretch never counts against the codec.
        assert!(!s.poll(false, false, t0 + ms(8000)));
        // The first AU after the pause starts the clock instead of tripping on the pause.
        let t1 = t0 + ms(10_000);
        assert!(!s.poll(false, true, t1));
        assert!(!s.poll(false, true, t1 + INPUT_STALL_PATIENCE - ms(1)));
        assert!(s.poll(false, true, t1 + INPUT_STALL_PATIENCE));
        // One freed slot restarts the wait.
        assert!(!s.poll(true, true, t1 + INPUT_STALL_PATIENCE + ms(5)));
        assert!(!s.poll(false, true, t1 + INPUT_STALL_PATIENCE + ms(10)));
    }
}
