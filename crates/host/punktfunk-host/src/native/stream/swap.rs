//! The one pipeline swap. A stream replaces its encoder, and often its capture with it, on a
//! session switch, a mode switch, a topology re-assert, a capture loss, a source that changed
//! size, and a bitrate the encoder cannot take in place. Each path builds the replacement and
//! hands it to [`StreamState::swap_pipeline`] with its [`SwapReason`].
//!
//! The swap owns what restarts with an encoder: the stream epoch, the in-flight set, the
//! watchdog, the IDR clock, the pipelined escalation, the encoder ceiling, the retarget state,
//! `enc_src`, and the display lease the new pipeline replaced. What a reason leaves alone is a
//! method of [`SwapReason`] and is decided nowhere else. A site keeps its mode, its rate and
//! what it tells the client.

use super::pipeline::Pipeline;
use super::state::StreamState;

/// Why the pipeline is swapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SwapReason {
    /// Gaming↔Desktop: a new display on another backend.
    SessionSwitch,
    /// The client's accepted mode.
    ModeSwitch,
    /// An exclusive-topology eviction bounced the display's modes.
    #[cfg_attr(not(windows), allow(dead_code, reason = "Windows only"))]
    TopologyReassert,
    CaptureLost,
    /// The source changed format or size under the live capturer.
    SourceMode,
    /// A rate the encoder cannot take in place.
    Bitrate,
}

impl SwapReason {
    /// A rate rebuild keeps the picture, so the client keeps what it holds.
    fn moves_epoch(self) -> bool {
        self != SwapReason::Bitrate
    }

    /// The geometry is new whatever the frames say: the encoder's limits are unknown again.
    fn new_geometry(self) -> bool {
        matches!(self, SwapReason::ModeSwitch | SwapReason::SourceMode)
    }

    /// The new encoder has been handed no rate yet. An encoder swapped alone keeps the
    /// retarget state it had, except on a mode switch.
    fn forgets_retarget(self, with_capture: bool) -> bool {
        with_capture || self == SwapReason::ModeSwitch
    }

    /// The display generation to release once the new pipeline is up: the one it replaced.
    /// A lease drop looks like a disconnect to keep-alive, so a replaced display would
    /// linger. A session switch's old display is another backend's, and the session watcher
    /// reaps those.
    pub(super) fn retires(self, old: Option<u64>, live: Option<u64>) -> Option<u64> {
        old.filter(|g| self != SwapReason::SessionSwitch && live != Some(*g))
    }

    /// `enc_src` follows the frame. Left stale, `follow_source_mode` reopens the encoder at
    /// what the source delivers and tells the client: a session switch relies on that, and a
    /// rate rebuild leaves a source change it did not make to it.
    fn rereads_enc_src(self) -> bool {
        self != SwapReason::SessionSwitch && self != SwapReason::Bitrate
    }

    /// The new encoder opened on an IDR, so the keyframe cooldown counts from now. A session
    /// switch keeps the clock's last stamp.
    fn arms_idr_clock(self) -> bool {
        self != SwapReason::SessionSwitch
    }
}

/// What a swap installs.
pub(super) enum Next {
    /// Capture and encoder both, from a build or a re-attach.
    Pipeline(Pipeline),
    /// The encoder alone, under a capturer that keeps its attachment. `resized` is the first
    /// frame at a new size and its pacing interval, after the Windows in-place mode set.
    Encoder {
        enc: Box<dyn crate::encode::Encoder>,
        reframe: punktfunk_core::video_fit::Reframe,
        resized: Option<(crate::capture::CapturedFrame, std::time::Duration)>,
    },
}

/// What an encoder is opened for: a change in any of the three is another encoder.
fn enc_src_of(frame: &crate::capture::CapturedFrame) -> (pf_frame::PixelFormat, u32, u32) {
    (frame.format, frame.width, frame.height)
}

impl StreamState {
    /// Install `next` and restart what restarts with an encoder. The caller closed any open
    /// wire frame before it opened that encoder ([`Self::close_open_frame`]).
    pub(super) fn swap_pipeline(&mut self, next: Next, why: SwapReason) {
        let old_gen = self.cur_display_gen;
        if why.moves_epoch() {
            self.epoch = self.epoch.wrapping_add(1);
        }
        // A ceiling was learned from the encoder this one replaces. It survives a swap that
        // opens on the same source.
        let (with_capture, source_moved) = match &next {
            Next::Pipeline(p) => (true, enc_src_of(&p.frame) != enc_src_of(&self.frame)),
            Next::Encoder { .. } => (false, false),
        };
        if source_moved || why.new_geometry() {
            self.encoder_ceiling
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
        if why.forgets_retarget(with_capture) {
            self.retargeted = false;
            self.fec.pending = None;
        }
        match next {
            Next::Pipeline(p) => self.adopt_pipeline(p),
            Next::Encoder {
                enc,
                reframe,
                resized,
            } => {
                self.adopt_reframe(reframe);
                self.enc = enc;
                self.carry_pipelining();
                if let Some((frame, interval)) = resized {
                    self.frame = frame;
                    self.interval = interval;
                }
            }
        }
        self.clock.inflight.clear();
        self.clock.watchdog.on_au();
        if let Some(g) = why.retires(old_gen, self.cur_display_gen) {
            crate::vdisplay::registry::retire(g);
        }
        if why.rereads_enc_src() {
            self.enc_src = enc_src_of(&self.frame);
        }
        if why.arms_idr_clock() {
            self.clock.last_forced_idr = Some(std::time::Instant::now());
        }
    }

    /// The capture side of a built pipeline, then its encoder. A new capturer numbers its
    /// cursor shapes from 1 again, so the last forwarded one is forgotten, and the clipboard
    /// follows the new capture's gamescope.
    fn adopt_pipeline(&mut self, p: Pipeline) {
        let Pipeline {
            capturer,
            enc,
            frame,
            interval,
            node_id,
            display_gen,
            bitrate_kbps: _,
            reframe,
            #[cfg(target_os = "linux")]
            lease,
            #[cfg(target_os = "linux")]
            panel_tick_ns,
        } = p;
        self.adopt_reframe(reframe);
        // The old capturer goes before the old encoder, as on teardown.
        self.capturer = capturer;
        if let Some(fwd) = self.cursor_fwd.as_mut() {
            *fwd = super::super::cursor_fwd::CursorForwarder::new();
        }
        self.enc = enc;
        self.carry_pipelining();
        self.frame = frame;
        self.interval = interval;
        self.cur_node_id = node_id;
        self.cur_display_gen = display_gen;
        #[cfg(target_os = "linux")]
        {
            super::state::publish_gamescope_xwayland(&self.gamescope_xwayland, lease.as_ref());
            self.lease = lease;
            self.restamp = panel_tick_ns.map(super::encode::SourceStamps::new);
        }
    }

    /// A fresh encoder retrieves synchronously. The session's pipelined escalation carries onto
    /// it, or the flags claim a mode the encoder is not in and escalation never fires again.
    fn carry_pipelining(&mut self) {
        if self.pipelining.deescalating {
            // The wind-back the old encoder owed: the new one is already there.
            self.pipelining.deescalating = false;
            self.pipelining.pipelined_active = false;
            self.pipelining.pipeline_asked = false;
        } else if self.pipelining.pipelined_active {
            self.pipelining.pipelined_active = self.enc.set_pipelined(true);
        } else {
            // A refusal was the old encoder's; this one may say yes.
            self.pipelining.pipeline_asked = false;
        }
    }
}
