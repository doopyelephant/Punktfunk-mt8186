//! The source contract the stream loops hold: [`Capturer`], the roles a source may expose, the
//! events it raises, and the host-built closures a Windows source sends through.
//!
//! Every source implements the core. A feed, a cursor, display control, supervision and a
//! remote encoder are roles, each reached through an accessor that defaults to `None`. A method
//! that one kind of source needs belongs on a role, never on the core: `docs/adr/0001`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use pf_frame::health::EncoderTelemetry;
use pf_frame::recovery::{Stage, StageOutcome};
use pf_frame::{CapturedFrame, CursorOverlay, HdrMeta};

/// A FATAL capture fault: retrying `try_latest` cannot help — the caller must rebuild the
/// capture attachment or fail the session. Carried inside the `anyhow::Error` a capture call
/// returns (downcast to route on it), so it can never collapse into an ordinary `Ok(None)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureFault {
    /// A known-ACTIVE display (input/cursor moving) delivered no new source frame through the
    /// stale floor and the staged recovery ladder (immunity plan WP13) — its terminal verdict;
    /// `secs` is the source gap at that point.
    SourceStalled { secs: u32 },
}

impl std::fmt::Display for CaptureFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceStalled { secs } => write!(
                f,
                "no source frame for {secs}s on a known-active display, through a rebuild"
            ),
        }
    }
}

impl std::error::Error for CaptureFault {}

/// The capturer's live health for the operator surface (immunity plan WP18): the classifier's
/// last verdict, the ring's self-report, and the last recovery episode. Plain data, no I/O —
/// the management layer maps it into its wire shape. Names are the classifier's own, lowercased.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureHealth {
    /// `healthy` / `idle` / `suspect` / `stalled` / `recovering` / `rebuilding` / `secure_desktop`.
    pub class: &'static str,
    /// The stall class when `class == "stalled"`: `worker` / `encoder` / `presentation` /
    /// `driver`.
    pub stall_class: Option<&'static str>,
    /// Time since the last real source frame.
    pub source_gap: std::time::Duration,
    /// The evidence the verdict rests on: `input` / `canary`.
    pub evidence: Option<&'static str>,
    /// The newest access unit's OS present stamp against the moment the host took it.
    pub present_to_arrival: Option<std::time::Duration>,
    /// `present_to_arrival` is past the classifier's bound: frames arrive late rather than not
    /// at all. A reported degradation — no recovery rung fires on it.
    pub late_frames: bool,
    /// The driver encoder's own state word: `closed` / `open` / `encoding` / `wedged`.
    /// `None` until the first `SET_ENCODE`.
    pub encoder_state: Option<&'static str>,
    /// The backend the driver opened (`nvenc` / `amf` / `qsv` / `pyrowave`); `None` as above.
    pub backend_opened: Option<&'static str>,
    /// Encode threads the driver abandoned after a wedge. Two opens the driver cycle.
    pub detached: u32,
    /// Access units the driver published, and frames it dropped at the encode pool.
    pub published_total: u64,
    pub dropped_total: u64,
    /// Frames the drain worker handed the pool — DWM's compose count, the source clock.
    pub source_seq: u64,
    /// The recovery stage running now, if an episode is open.
    pub current_stage: Option<&'static str>,
    /// The last closed episode.
    pub last_episode: Option<CaptureEpisode>,
    /// Stalled verdicts refused for budget or cooldown since the last episode.
    pub episodes_suppressed: u32,
    /// Time left in the post-failure cooldown.
    pub cooldown_remaining: Option<std::time::Duration>,
}

/// One closed recovery episode, as [`CaptureHealth`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureEpisode {
    pub stall_class: &'static str,
    pub recovered: bool,
    pub took: std::time::Duration,
    /// `(stage, outcome, took)` in ladder order; `outcome` is `applied` / `failed` /
    /// `unsupported` / `timed_out`.
    pub stages: Vec<(&'static str, &'static str, std::time::Duration)>,
    pub consecutive_failures: u32,
    pub cooldown: std::time::Duration,
}

/// Context on a capture loss whose display is still up (the import side broke, not the
/// compositor): the host may re-attach a capturer to the same output instead of creating
/// another one — on KWin every create is a new virtual output, and a burst of them wedges it.
#[derive(Debug, Clone, Copy)]
pub struct DisplayStillAlive;

impl std::fmt::Display for DisplayStillAlive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the output itself is still up")
    }
}

/// What a stream loop holds. A pixel source hands it pixels; a remote-encoder source hands it
/// the news that an encoder it does not run took a frame (`FramePayload::Pixelless`).
pub trait Capturer: Send {
    /// A frame within `budget`, blocking. The loop takes an attach's first frame this way; a
    /// synthetic source answers every call.
    fn next_frame(&mut self, budget: FirstFrame) -> Result<CapturedFrame>;

    /// Non-blocking: the freshest frame since the last call, or `None` so the loop repeats its
    /// last frame and holds its output rate. Default: a new frame each call.
    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.next_frame(FirstFrame::Default).map(Some)
    }

    /// Whether this source can still produce frames, without consuming one. A pool consults it
    /// before reuse: a poisoned import or a dead producer fails every later call.
    fn is_alive(&self) -> bool {
        true
    }

    /// What the loop must act on since the last call, oldest first.
    fn events(&mut self) -> Vec<SourceEvent> {
        Vec::new()
    }

    /// The colour the source delivers now. Live: an HDR flip or a regrade changes it.
    fn colour(&self) -> ColourFacts {
        ColourFacts::default()
    }

    /// [`Feed`]: PipeWire and direct Wayland capture.
    fn feed(&mut self) -> Option<&mut dyn Feed> {
        None
    }

    /// [`CursorSource`]: PipeWire and IDD-push.
    fn cursor(&mut self) -> Option<&mut dyn CursorSource> {
        None
    }

    /// [`DisplayControl`]: IDD-push.
    fn display(&mut self) -> Option<&mut dyn DisplayControl> {
        None
    }

    /// [`Supervised`]: IDD-push and WGC.
    fn supervised(&mut self) -> Option<&mut dyn Supervised> {
        None
    }

    /// [`RemoteEncoder`]: IDD-push and WGC.
    fn remote_encoder(&self) -> Option<&dyn RemoteEncoder> {
        None
    }
}

/// How long [`Capturer::next_frame`] may wait, and what its expiry means. A remote-encoder or
/// synthetic source has no offer to convict and waits its own way whatever is asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirstFrame {
    /// The source's full wait: 10 s for a Linux pixel source, 20 s for a remote encoder.
    Default,
    /// At most this long; expiry judges the offer as `Default`'s does.
    Within(Duration),
    /// At most this long, and expiry convicts no offer: a cold producer can outlive a short
    /// first attempt and still accept every offer, so no HDR or dmabuf downgrade latches.
    Provisional(Duration),
}

/// What a source asks of the loop, drained by [`Capturer::events`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceEvent {
    /// The frame the encoder last read may be torn: the producer re-sent a buffer this side
    /// still held. The loop answers with one IDR.
    ReferenceRisk,
    /// A staged-recovery episode closed on new source frames: the local outage, from the last
    /// frame before the stall to the one that proved recovery. The loop forces an IDR and
    /// announces the gap.
    RecoveredOutage(Duration),
    /// A recovery rung whose actuator the loop owns, because it owns the encoder
    /// (`EncoderReset`) or the display manager does (`DriverCycle`). The loop runs it and
    /// answers through [`Supervised::stage_done`].
    RunStage(Stage),
}

/// The colour a source delivers, as a value the loop reads at encoder open and every tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColourFacts {
    /// Static HDR mastering metadata while the source delivers PQ, `None` for SDR. Direct
    /// Wayland capture sends the output's own volume; every other source the generic HDR10
    /// block, as neither OS exposes a real one through capture.
    pub hdr_meta: Option<HdrMeta>,
    /// The display composes FP16 under an SDR transfer (Windows SDR wide colour), so an SDR
    /// stream's encoder reads FP16.
    pub sdr_fp16: bool,
}

/// A source whose producer runs on its own thread into a mailbox: PipeWire and direct Wayland
/// capture. The loop paces on its arrivals instead of a fixed tick, pauses it between streams,
/// and takes back the output it holds for a capture-only rebuild.
pub trait Feed {
    /// Block until a frame is ready for [`Capturer::try_latest`] or `deadline` passes. Never
    /// consumes the frame; an error surfaces at the following `try_latest`.
    fn wait_arrival(&mut self, deadline: Instant);

    /// A handle that ends a running [`Self::wait_arrival`] now, with no frame, so a recovery
    /// ask from the control task goes out at once. `None`: the ask waits for the next tick.
    fn arrival_poke(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        None
    }

    /// Gate the producer's per-frame work. A PipeWire stream drops every buffer until `true`;
    /// `false` also flushes the mailbox, so a pooled source never hands the next stream the
    /// last one's frame. Default: always active.
    fn set_active(&mut self, _active: bool) {}

    /// The output's keepalive, so a capture-only rebuild re-attaches to the live output.
    /// `None` when the feed holds none or gave it up; the output then goes with the source.
    fn take_keepalive(&mut self) -> Option<Box<dyn Send>> {
        None
    }
}

/// A source that sees the pointer apart from its frames: PipeWire (`SPA_META_Cursor`, or
/// XFixes on gamescope) and IDD-push (the IddCx hardware-cursor channel). The loop forwards the
/// overlay to a client that draws the pointer, or puts it on the frame for the encoder blend.
pub trait CursorSource {
    /// The live pointer. Preferred over `CapturedFrame::cursor`: a pointer-only move produces
    /// no frame, so a frame's overlay goes stale.
    fn overlay(&mut self) -> Option<CursorOverlay>;

    /// `true`: the client draws the pointer, so it stays out of the video; `false`: back in. A
    /// declared IddCx hardware cursor is irrevocable. On Linux either call means the host
    /// places the pointer, so a CPU copy stops baking it in.
    fn set_forward(&mut self, client_draws: bool);
}

/// The captured display is the host's to mode-set in place: IDD-push. The native loop resizes
/// and restores presentation through it instead of rebuilding the pipeline.
pub trait DisplayControl {
    /// The OS display target this source is bound to. A resize that lands on another target
    /// (a re-arrival) is a rebuild.
    fn target_id(&self) -> u32;

    /// Adopt the mode the session handler already committed: no descriptor debounce, no
    /// teardown. `false`: rebuild.
    fn resize(&mut self, width: u32, height: u32) -> bool;

    /// Make the OS present to this display again at its current mode, after an exclusive
    /// topology left it active but unpresented. `false`: unrecoverable in place.
    fn restart_presentation(&mut self) -> bool;
}

/// A source a supervisor classifies: IDD-push (classify, and run the recovery ladder) and WGC
/// (classify only: a quiet mirror is idle, never a stall). The loop feeds it the encoder's
/// clocks each tick, publishes its health, and answers the rungs it raises.
pub trait Supervised {
    /// The session encoder's clocks, once per tick before `try_latest`.
    fn observe(&mut self, telemetry: Option<EncoderTelemetry>);

    /// Live health for the operator surface.
    fn health(&self) -> CaptureHealth;

    /// The loop-owned actuator for a [`SourceEvent::RunStage`] finished with `outcome`. A
    /// source that runs no ladder raises none.
    fn stage_done(&mut self, _stage: Stage, _outcome: StageOutcome) {}
}

/// The source's encoder runs in another process behind an AU section: the pf-vdisplay driver
/// in WUDFHost (IDD-push) or the capture worker (WGC). The loop opens its encoder here, never a
/// local backend.
pub trait RemoteEncoder {
    /// Where the encoder runs, for the session record's label.
    fn site(&self) -> EncoderSite;

    /// Open the encoder for `params` and hand back the loop's proxy. No fallback follows: a
    /// refused open ends the session (the driver's own refusal is `DriverEncodeOpenError`).
    fn open(&self, params: &DriverEncodeParams) -> Result<Box<dyn pf_encode_core::Encoder>>;
}

/// The process a [`RemoteEncoder`] encodes in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderSite {
    Driver,
    Worker,
}

/// What the host asks a [`RemoteEncoder`] to open — the session plan as `SET_ENCODE` numbers it.
#[derive(Clone, Copy, Debug)]
pub struct DriverEncodeParams {
    /// 1 H264, 2 HEVC, 3 AV1, 4 PyroWave.
    pub codec: u32,
    /// `0` 4:2:0, `1` 4:4:4.
    pub chroma: u32,
    pub bit_depth: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub hdr: bool,
    pub hdr_meta: Option<HdrMeta>,
    /// The display composes FP16 for this SDR stream (SDR wide colour):
    /// [`pf_driver_proto::encode::SET_ENCODE_FLAG_SDR_FP16`].
    pub sdr_fp16: bool,
    /// H.264/HEVC slices per frame the client takes:
    /// [`pf_driver_proto::encode::SetEncodeRequest::slices`].
    pub slices: u32,
    /// Slice-chunk target; `0` = whole access units.
    pub wire_chunk_bytes: u32,
    /// Ordered preference, 0-terminated: 1 NVENC, 2 AMF, 3 QSV, 4 PyroWave.
    pub backends: [u32; 4],
    /// First `wire_seq` the driver stamps — the host's current `au_seq`.
    pub wire_seq_base: u32,
}

/// Discovers gamescope's nested Xwayland cursor targets — `(DISPLAY, XAUTHORITY)`,
/// one per `--xwayland-count` — for [`crate::VirtualOutputOpts::gamescope_cursor`].
///
/// A closure, re-run on a slow cadence: gamescope creates a second Xwayland for
/// the game but advertises only the first in any child's environ, so a one-shot
/// snapshot taken before launch never sees the game display.
///
/// Built by the host facade (`pf_vdisplay::gamescope_xwayland_cursor_targets`)
/// so the capture→host edge stays one-way.
#[cfg(target_os = "linux")]
pub type GamescopeCursorTargets =
    std::sync::Arc<dyn Fn() -> Vec<(String, Option<String>)> + Send + Sync>;

/// v5 hardware-cursor channel (`IOCTL_SET_CURSOR_CHANNEL`). Host-built so this
/// crate never reaches the orchestrator; on IOCTL success the driver owns the
/// handle duplicated into WUDFHost. `Some` opts in: the capturer creates
/// the cursor section only when the host hands a sender; a plain session
/// keeps DWM's pointer.
#[cfg(target_os = "windows")]
pub type CursorChannelSender = std::sync::Arc<
    dyn Fn(&pf_driver_proto::control::SetCursorChannelRequest) -> Result<()> + Send + Sync,
>;

/// Mid-stream cursor-render flip (`IOCTL_SET_CURSOR_FORWARD`). `true` declares
/// the IddCx hardware cursor; `false` stands it down (host also forces the
/// same-mode re-commit that actualises the OS software cursor). UAC/Winlogon
/// render only through software cursor.
#[cfg(target_os = "windows")]
pub type CursorForwardSender = std::sync::Arc<dyn Fn(bool) -> Result<()> + Send + Sync>;

/// v7 in-driver encode open (`IOCTL_SET_ENCODE`) — same facade contract as
/// `CursorChannelSender`: the driver adopts the request's handle values iff the
/// IOCTL succeeds. Once per encoder generation.
pub type SetEncodeSender = std::sync::Arc<
    dyn Fn(
            &pf_driver_proto::encode::SetEncodeRequest,
        ) -> Result<pf_driver_proto::encode::SetEncodeReply>
        + Send
        + Sync,
>;

/// v7 one-shot encoder control (`IOCTL_ENCODE_CTL`): the `Encoder` calls the
/// stream loop makes, forwarded by the driver proxy.
pub type EncodeCtlSender =
    std::sync::Arc<dyn Fn(&pf_driver_proto::encode::EncodeCtlRequest) -> Result<()> + Send + Sync>;
