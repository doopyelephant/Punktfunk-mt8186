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
//!
//! Pinned by `each_swap_restarts_what_its_reason_says`, one row per site.

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

    /// `enc_src` follows the frame where the site publishes the delivered mode itself, or
    /// rebuilds at the mode it had. A session switch and a capture loss can land on a backend
    /// that delivers another size: left stale, `follow_source_mode` sees it, sets `cur_mode`,
    /// re-resolves the rate and tells the client. A rate rebuild leaves a source change it did
    /// not make to it too.
    fn rereads_enc_src(self) -> bool {
        match self {
            SwapReason::ModeSwitch | SwapReason::TopologyReassert | SwapReason::SourceMode => true,
            SwapReason::SessionSwitch | SwapReason::CaptureLost | SwapReason::Bitrate => false,
        }
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
        // The new encoder opened on an IDR: the keyframe cooldown counts from it.
        self.clock.last_forced_idr = Some(std::time::Instant::now());
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

#[cfg(test)]
mod tests {
    use super::super::state::{AuClock, FecState, Pipelining};
    use super::*;
    use crate::capture::{CapturedFrame, Capturer, SyntheticCapturer};
    use crate::native::bitrate::EncoderCeiling;
    use punktfunk_core::quic::AckReason;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const SMALL: (u32, u32) = (16, 16);
    const WIDE: (u32, u32) = (32, 16);
    const INTERVAL: Duration = Duration::from_millis(16);

    /// Encodes nothing.
    struct NullEnc;

    impl crate::encode::Encoder for NullEnc {
        fn submit(&mut self, _frame: &CapturedFrame) -> anyhow::Result<()> {
            Ok(())
        }
        fn poll(&mut self) -> anyhow::Result<Option<crate::encode::EncodedFrame>> {
            Ok(None)
        }
        fn flush(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct NullDisplay;

    impl crate::vdisplay::VirtualDisplay for NullDisplay {
        fn name(&self) -> &'static str {
            "null"
        }
        fn create(
            &mut self,
            _mode: pf_vdisplay::Mode,
        ) -> anyhow::Result<crate::vdisplay::VirtualOutput> {
            anyhow::bail!("these tests never create a display")
        }
    }

    fn frame((w, h): (u32, u32)) -> CapturedFrame {
        SyntheticCapturer::new(w, h, 60)
            .next_frame()
            .expect("a synthetic frame")
    }

    /// A built pipeline of `size` on display generation 2.
    fn pipeline(size: (u32, u32)) -> Next {
        Next::Pipeline(Pipeline {
            capturer: Box::new(SyntheticCapturer::new(size.0, size.1, 60)),
            enc: Box::new(NullEnc),
            frame: frame(size),
            interval: INTERVAL,
            node_id: 2,
            display_gen: Some(2),
            bitrate_kbps: 20_000,
            reframe: Default::default(),
            #[cfg(target_os = "linux")]
            lease: None,
            #[cfg(target_os = "linux")]
            panel_tick_ns: None,
        })
    }

    /// An encoder alone, with the first frame of `resized` when the display was resized in place.
    fn encoder(resized: Option<(u32, u32)>) -> Next {
        Next::Encoder {
            enc: Box::new(NullEnc),
            reframe: Default::default(),
            resized: resized.map(|size| (frame(size), INTERVAL)),
        }
    }

    /// A stream loop's state with nothing behind it, and what keeps its link valid. Field
    /// order is drop order: the state goes before the runtime its connection lives on.
    struct Fake {
        st: StreamState,
        _client: quinn::Connection,
        _endpoints: (quinn::Endpoint, quinn::Endpoint),
        _rt: tokio::runtime::Runtime,
    }

    /// A session mid-stream on display generation 1 at [`SMALL`], with everything a swap
    /// restarts left dirty: two AUs in flight, a spent watchdog, no IDR stamp, a retarget and
    /// a FEC ask pending, a learned ceiling, and a pipelined retrieve being wound back.
    fn fake() -> Fake {
        use punktfunk_core::quic::endpoint;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let (server, client, host_conn, client_conn) = rt.block_on(async {
            let server = endpoint::server("127.0.0.1:0".parse().unwrap()).expect("server");
            let addr = server.local_addr().expect("server address");
            let client = endpoint::client_insecure().expect("client");
            let (host_conn, client_conn) = tokio::join!(
                async { server.accept().await.expect("incoming").await },
                async { client.connect(addr, "punktfunk").expect("dial").await },
            );
            (
                server,
                client,
                host_conn.expect("host side connects"),
                client_conn.expect("client side connects"),
            )
        });
        let conn = crate::native::link::SessionLink::QuicV2(
            host_conn.clone(),
            Arc::new(crate::native::link::V2Link::new(
                host_conn,
                Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").expect("media socket")),
            )),
        );
        let mode = punktfunk_core::Mode {
            width: SMALL.0,
            height: SMALL.1,
            refresh_hz: 60,
        };
        let now = Instant::now();
        let quit = Arc::new(AtomicBool::new(false));
        let live = frame(SMALL);
        let mut ceiling = EncoderCeiling::new();
        ceiling.note_applied(50_000, 30_000);
        let mut watchdog = crate::encode_recovery::EncoderWatchdog::new();
        watchdog.spend(INTERVAL);
        let (frame_tx, _) = std::sync::mpsc::sync_channel(1);
        let st = StreamState {
            deadline: now + Duration::from_secs(60),
            next: now,
            sent: 0,
            pace: crate::send_pacing::CaptureCredit::new(now),
            clock: AuClock {
                au_seq: 40,
                wire_frame_open: false,
                inflight: [(1, 1), (2, 2)].into(),
                watchdog,
                last_forced_idr: None,
            },
            capture_rebuilds: 0,
            seen_reassert_gen: 0,
            last_hdr_meta: None,
            diag_new: 0,
            diag_repeat: 0,
            diag_regen: 0,
            diag_at: now,
            #[cfg(target_os = "linux")]
            parked_display: None,
            #[cfg(target_os = "linux")]
            park_attempts: 0,
            #[cfg(target_os = "linux")]
            next_park_at: now,
            composite_log: Default::default(),
            last_rfi: None,
            loss_mode: Default::default(),
            acked: Default::default(),
            lossy_window: Default::default(),
            kf_gate: Default::default(),
            recovery_cadence: pf_frame::metronome::Metronome::new(),
            ir_wave_pos: 0,
            st_cap: Vec::new(),
            st_submit: Vec::new(),
            st_wait: Vec::new(),
            st_queue: Vec::new(),
            driver_dropped: Default::default(),
            pipelining: Pipelining {
                cur_depth: 1,
                behind_score: 0,
                depth_frames: 0,
                pipeline_asked: true,
                pipelined_active: true,
                deescalating: true,
                ahead_run: 0,
                deescalate_not_before: None,
                deescalate_backoff: Duration::from_secs(60),
            },
            fec: FecState {
                requested: Arc::new(AtomicU8::new(20)),
                target: Arc::new(AtomicU8::new(10)),
                last: 10,
                pending: Some((20, 15_000)),
                hold_logged: 0,
            },
            src_period_ns: None,
            encode_chain_ns: 0,
            last_real_cap: None,
            was_degraded: false,
            last_cadence_log: None,
            cadence_flips_suppressed: 0,
            _watcher: None,
            #[cfg(target_os = "linux")]
            seat_join: None,
            live_session: crate::session_status::register(
                crate::session_status::Registration::fake("swap-test"),
            ),
            frame_tx: Arc::new(frame_tx),
            send_thread: std::thread::spawn(|| {}),
            send_spread_us: Default::default(),
            wire_rekeys: Default::default(),
            live_mode: Arc::new(AtomicU64::new(0)),
            force_idr: Default::default(),
            capture_health: Default::default(),
            health_published_at: now,
            game_life: None,
            capturer: Box::new(SyntheticCapturer::new(SMALL.0, SMALL.1, 60)),
            enc: Box::new(NullEnc),
            enc_src: enc_src_of(&live),
            frame: live,
            interval: INTERVAL,
            cur_node_id: 1,
            cur_display_gen: Some(1),
            epoch: 7,
            #[cfg(target_os = "linux")]
            lease: None,
            #[cfg(target_os = "linux")]
            restamp: None,
            cur_mode: mode,
            bitrate_kbps: 20_000,
            vd: Box::new(NullDisplay),
            compositor: crate::vdisplay::Compositor::Kwin,
            cursor_fwd: None,
            cursor_client_drew: true,
            gamescope_composite: false,
            metadata_composite: false,
            #[cfg(target_os = "linux")]
            no_overlay_means_off_output: false,
            plan: crate::session_plan::SessionPlan {
                capture: crate::session_plan::CaptureBackend::Portal,
                topology: crate::session_plan::SessionTopology::SingleProcess,
                encoder: crate::session_plan::EncoderBackend::Software,
                bit_depth: 8,
                hdr: false,
                chroma: crate::encode::ChromaFormat::Yuv420,
                codec: crate::encode::Codec::H265,
                wire_chunk: None,
                pyrowave_bpp: 0.0,
                cursor_blend: false,
                cursor_forward: false,
                gamescope_cursor: false,
                sdr10_native: false,
                max_slices: 1,
                reframe_to: None,
            },
            stop: Default::default(),
            quit: quit.clone(),
            end_reason: Default::default(),
            counters: Default::default(),
            conn,
            negotiated: mode,
            bitrate_auto: false,
            bit_depth: 8,
            audio_reserved_kbps: 0,
            shard_payload: 1200,
            budget_identity: false,
            streamed_wire: false,
            perf: false,
            vd_params: crate::vdisplay::SessionParams {
                client_fp: None,
                client_hdr: None,
                hdr: false,
                hw_cursor: false,
                join_live: false,
                quit,
                launch: None,
                route: None,
                isolation: None,
            },
            client_hdr: None,
            frame_map: Default::default(),
            #[cfg(target_os = "linux")]
            gamescope_xwayland: Default::default(),
            bringup: crate::bringup::Trace::start("swap-test", Arc::new(AtomicU32::new(0))),
            resize_ms: Default::default(),
            stats: crate::stats_recorder::StatsRecorder::new(
                std::env::temp_dir().join(format!("pf-swap-test-{}", std::process::id())),
            ),
            live_bitrate: Arc::new(AtomicU32::new(20_000)),
            encoder_ceiling: Arc::new(Mutex::new(ceiling)),
            retargeted: true,
            cadence_degraded: Default::default(),
            cadence_behind_score: Default::default(),
            client_packets_received: Default::default(),
            cursor_shape_tx: tokio::sync::watch::channel(None).0,
            cursor_client_draws: Default::default(),
            #[cfg(target_os = "linux")]
            input_tx: std::sync::mpsc::sync_channel(1).0,
            #[cfg(target_os = "linux")]
            input_route: crate::native::input::InputRoute::new(std::sync::mpsc::channel().0),
            #[cfg(target_os = "linux")]
            inj_shared_tx: std::sync::mpsc::channel().0,
            #[cfg(target_os = "linux")]
            inj_session_tx: None,
            reconfig: std::sync::mpsc::channel().1,
            keyframe: std::sync::mpsc::channel().1,
            rfi: std::sync::mpsc::channel().1,
            bitrate_rx: std::sync::mpsc::channel().1,
            session_rx: std::sync::mpsc::channel().1,
            reconfig_result_tx: tokio::sync::mpsc::unbounded_channel().0,
            retarget_tx: tokio::sync::mpsc::unbounded_channel().0,
            gap_tx: tokio::sync::mpsc::unbounded_channel().0,
        };
        Fake {
            st,
            _client: client_conn,
            _endpoints: (server, client),
            _rt: rt,
        }
    }

    /// What one swap site leaves behind. Everything not named here holds for every site: nothing
    /// in flight, the watchdog fed, the wind-back the old encoder owed settled, the IDR clock
    /// armed.
    struct Row {
        site: &'static str,
        why: SwapReason,
        next: fn() -> Next,
        /// The source moved under the old encoder before this swap, as `follow_source_mode` sees it.
        source_moved_first: bool,
        epoch_moved: bool,
        ceiling_cleared: bool,
        retarget_forgotten: bool,
        enc_src_reread: bool,
        retired: Option<u64>,
    }

    const ROWS: &[Row] = &[
        Row {
            site: "session switch",
            why: SwapReason::SessionSwitch,
            next: || pipeline(WIDE),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: true,
            retarget_forgotten: true,
            enc_src_reread: false,
            retired: None,
        },
        Row {
            site: "mode switch",
            why: SwapReason::ModeSwitch,
            next: || pipeline(SMALL),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: true,
            retarget_forgotten: true,
            enc_src_reread: true,
            retired: Some(1),
        },
        Row {
            site: "mode switch in place",
            why: SwapReason::ModeSwitch,
            next: || encoder(Some(WIDE)),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: true,
            retarget_forgotten: true,
            enc_src_reread: true,
            retired: None,
        },
        Row {
            site: "topology re-assert",
            why: SwapReason::TopologyReassert,
            next: || pipeline(SMALL),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: false,
            retarget_forgotten: true,
            enc_src_reread: true,
            retired: Some(1),
        },
        Row {
            site: "topology re-assert in place",
            why: SwapReason::TopologyReassert,
            next: || encoder(Some(SMALL)),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: false,
            retarget_forgotten: false,
            enc_src_reread: true,
            retired: None,
        },
        Row {
            site: "capture lost",
            why: SwapReason::CaptureLost,
            next: || pipeline(WIDE),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: true,
            retarget_forgotten: true,
            enc_src_reread: false,
            retired: Some(1),
        },
        Row {
            site: "source mode",
            why: SwapReason::SourceMode,
            next: || encoder(None),
            source_moved_first: true,
            epoch_moved: true,
            ceiling_cleared: true,
            retarget_forgotten: false,
            enc_src_reread: true,
            retired: None,
        },
        Row {
            site: "bitrate",
            why: SwapReason::Bitrate,
            next: || encoder(None),
            source_moved_first: true,
            epoch_moved: false,
            ceiling_cleared: false,
            retarget_forgotten: false,
            enc_src_reread: false,
            retired: None,
        },
    ];

    #[test]
    fn each_swap_restarts_what_its_reason_says() {
        let _registry = crate::session_status::tests::registry_lock();
        for row in ROWS {
            let site = row.site;
            let mut session = fake();
            let st = &mut session.st;
            if row.source_moved_first {
                st.frame = frame(WIDE);
            }
            let old_gen = st.cur_display_gen;

            st.swap_pipeline((row.next)(), row.why);

            assert!(st.clock.inflight.is_empty(), "{site}: nothing in flight");
            assert_eq!(st.clock.watchdog.resets(), 0, "{site}: watchdog fed");
            let p = &st.pipelining;
            assert_eq!(
                (p.deescalating, p.pipelined_active, p.pipeline_asked),
                (false, false, false),
                "{site}: escalation carried"
            );
            assert!(st.clock.last_forced_idr.is_some(), "{site}: IDR clock");
            assert_eq!(st.epoch != 7, row.epoch_moved, "{site}: epoch");
            let (_, ceiling) = st.encoder_ceiling.lock().unwrap().resolve(50_000);
            assert_eq!(
                ceiling == AckReason::Granted,
                row.ceiling_cleared,
                "{site}: ceiling"
            );
            assert_eq!(
                (st.retargeted, st.fec.pending.is_some()),
                (!row.retarget_forgotten, !row.retarget_forgotten),
                "{site}: retarget state"
            );
            assert_eq!(
                st.enc_src == enc_src_of(&st.frame),
                row.enc_src_reread,
                "{site}: enc_src"
            );
            assert_eq!(
                row.why.retires(old_gen, st.cur_display_gen),
                row.retired,
                "{site}: display lease"
            );
        }
    }

    /// A built pipeline brings its capture with it, and the AU numbering runs on: the new
    /// encoder opened at `au_seq`.
    #[test]
    fn a_built_pipeline_brings_its_capture_and_keeps_the_au_sequence() {
        let _registry = crate::session_status::tests::registry_lock();
        let mut session = fake();
        let st = &mut session.st;
        st.swap_pipeline(pipeline(WIDE), SwapReason::CaptureLost);
        assert_eq!(st.clock.au_seq, 40);
        assert_eq!((st.cur_node_id, st.cur_display_gen), (2, Some(2)));
        assert_eq!((st.frame.width, st.frame.height), WIDE);
    }
}
