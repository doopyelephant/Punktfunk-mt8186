//! The source contract the stream loops hold: [`Capturer`], its fault and health types,
//! and the host-built closures a Windows source sends through.

use anyhow::Result;
use pf_frame::CapturedFrame;

#[cfg(target_os = "windows")]
use crate::WorkerEndpoint;

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

/// Produces frames without blocking the compositor. The Linux portal publishes
/// into a one-deep overwriting slot (drop-oldest): a stalled consumer still
/// sees the freshest frame.
pub trait Capturer: Send {
    fn next_frame(&mut self) -> Result<CapturedFrame>;

    /// Hand the virtual output's keepalive back so a capture-only rebuild can re-attach to the
    /// live output. `None` when this capturer holds none, or already gave it up; the output is
    /// then released when the capturer drops, as before.
    fn take_keepalive(&mut self) -> Option<Box<dyn Send>> {
        None
    }

    /// [`next_frame`](Self::next_frame) with a caller-chosen first-frame budget.
    /// A PipeWire stream can sit in `Streaming` with no buffer; retry shortens
    /// the first wait instead of blocking the default. Backends without an
    /// internal wait budget ignore it (the default delegates).
    fn next_frame_within(&mut self, _budget: std::time::Duration) -> Result<CapturedFrame> {
        self.next_frame()
    }

    /// [`next_frame_within`](Self::next_frame_within) whose expiry is retry, not
    /// a capture verdict. Must not latch process-wide HDR/dmabuf-only downgrades:
    /// a cold start can outlive the short window and still accept every offer.
    /// Backends that latch nothing from a timeout just delegate.
    fn next_frame_within_provisional(
        &mut self,
        budget: std::time::Duration,
    ) -> Result<CapturedFrame> {
        self.next_frame_within(budget)
    }

    /// Non-blocking: the freshest frame since the last call, or `None` so the
    /// caller reuses its last frame and holds a steady output rate. Default
    /// produces a frame each call; the portal drains without blocking.
    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.next_frame().map(Some)
    }

    /// Whether [`wait_arrival`](Self::wait_arrival) is usable. `false` (default)
    /// keeps the encode loop on its fixed-cadence tick.
    fn supports_arrival_wait(&self) -> bool {
        false
    }

    /// Block until a frame is ready for [`try_latest`](Self::try_latest) or
    /// `deadline` passes. Must not consume the frame. Only called when
    /// [`supports_arrival_wait`](Self::supports_arrival_wait) is `true`;
    /// errors surface at the following `try_latest`.
    fn wait_arrival(&mut self, _deadline: std::time::Instant) {}

    /// A handle that ends a running [`wait_arrival`](Self::wait_arrival) now, with no
    /// frame, so the loop ticks on the frame it has. For the control task: a client's
    /// recovery ask goes out at once instead of after the next arrival. `None` (default)
    /// leaves the ask for the next tick.
    fn arrival_poke(&self) -> Option<std::sync::Arc<dyn Fn() + Send + Sync>> {
        None
    }

    /// Gate expensive per-frame work so the capturer can stay alive between
    /// streams. The portal skips the de-pad copy while inactive and flushes
    /// its frame mailbox on `false`. `&mut self`: the mailbox flush cannot share.
    fn set_active(&mut self, _active: bool) {}

    /// Whether this capturer can still produce frames. A pool must consult this
    /// before reuse: zero-copy poison, a dead PipeWire thread, or a source that
    /// never returns to `Streaming` makes every later call fail. Default `true`.
    fn is_alive(&self) -> bool {
        true
    }

    /// Live cursor out-of-band from frames (Windows IddCx hardware-cursor
    /// channel). Preferred over `CapturedFrame::cursor`: pointer-only moves
    /// produce no frame, so a frame-attached overlay goes stale. Default `None`.
    fn cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        None
    }

    /// Cursor-render flip: `true` keeps the pointer out of the video (client
    /// draws it); `false` puts it back in. A declared IddCx hardware cursor is
    /// irrevocable — DWM cannot take the job back. On Linux either call means
    /// the host places [`cursor`](Self::cursor), so a CPU copy stops baking it.
    /// Default no-op.
    fn set_cursor_forward(&mut self, _on: bool) {}

    /// Attach a gamescope cursor source. gamescope paints no `SPA_META_Cursor`,
    /// so [`cursor`](Self::cursor) stays empty unless the portal reads nested
    /// Xwaylands (one per `--xwayland-count`) over X11. Called once, after build.
    #[cfg(target_os = "linux")]
    fn attach_gamescope_cursor(&mut self, _targets: GamescopeCursorTargets) {}

    /// Static HDR mastering metadata (SMPTE ST.2086 + CLL) when the capturer can
    /// read it (Windows `IDXGIOutput6::GetDesc1`), or a generic HDR10 block once
    /// an HDR stream is negotiated (Linux exposes no real mastering volume).
    /// Forwarded to the encoder (SEI) and the client (`0xCE`). May change if regraded.
    fn hdr_meta(&self) -> Option<pf_frame::HdrMeta> {
        None
    }

    /// The display composes FP16 under an SDR transfer (Windows SDR wide colour), so an SDR
    /// stream's encoder reads FP16. Default `false`.
    fn composes_sdr_fp16(&self) -> bool {
        false
    }

    // `capture_target_id` and `resize_output` are one operation split in half:
    // `Some` from the id promises resize works; resize without the id cannot
    // check the reconfigured display is still this capturer's. Both defaults decline.

    /// OS display-target id this capturer is bound to (Windows IDD-push). Resize
    /// uses it to verify the reconfigured display is still this one. In-place
    /// resize keeps the target; a re-arrival fallback mints a new one. `None`
    /// = no such identity.
    fn capture_target_id(&self) -> Option<u32> {
        None
    }

    /// Host-initiated output resize after the session handler has committed the
    /// new mode. Resize the capture surface now: no descriptor-poll debounce,
    /// no teardown. `true` handled; `false` rebuilds.
    fn resize_output(&mut self, _width: u32, _height: u32) -> bool {
        false
    }

    /// Make the OS present to this display again at the current mode. An
    /// exclusive-topology eviction leaves the target active but unpresented, so the
    /// descriptor never changes and the two-strike debounce never trips. `true`
    /// handled; `false` unrecoverable.
    fn restart_presentation_in_place(&mut self) -> bool {
        false
    }

    /// A staged-recovery episode closed on new source frames since the last
    /// call: the measured local outage, from the last source frame before the
    /// stall to the frame that proved recovery. The stream loop forces an IDR
    /// and announces the gap. `None` = nothing recovered.
    fn take_recovered_outage(&mut self) -> Option<std::time::Duration> {
        None
    }

    /// Since the last call, the frame the encoder last read may be torn (a
    /// producer re-sent a buffer this side still held). The stream loop
    /// answers with one IDR. Default: never.
    fn take_reference_risk(&mut self) -> bool {
        false
    }

    /// Live capture health for the operator surface (WP18). `None` = this
    /// capturer does not classify (Linux portal, synthetic sources).
    fn health(&self) -> Option<CaptureHealth> {
        None
    }

    /// The session encoder's own clocks (`Encoder::telemetry`), handed over
    /// once per loop tick before `try_latest` so the supervisor classifies the
    /// encode leg on them. Default: ignored.
    fn observe_encoder(&mut self, _t: Option<pf_frame::health::EncoderTelemetry>) {}

    /// A recovery rung whose actuator the stream loop owns because the
    /// encoder does (`EncoderReset`) or the display manager does
    /// (`DriverCycle`). The loop runs it and answers with [`Self::stage_done`].
    /// Default: never.
    fn take_pending_stage(&mut self) -> Option<pf_frame::recovery::Stage> {
        None
    }

    /// The loop-owned actuator for `stage` finished with `outcome`.
    fn stage_done(
        &mut self,
        _stage: pf_frame::recovery::Stage,
        _outcome: pf_frame::recovery::StageOutcome,
    ) {
    }

    /// The monitor and WUDFHost an in-driver encoder opens against
    /// ([`open_driver_encoder`](crate::open_driver_encoder)). `None` = not an IDD-push source.
    #[cfg(target_os = "windows")]
    fn driver_endpoint(&self) -> Option<DriverEndpoint> {
        None
    }

    /// The capture worker this session's encoder opens in
    /// ([`open_worker_encoder`](crate::open_worker_encoder)). `None` = the source is not a
    /// worker's.
    #[cfg(target_os = "windows")]
    fn worker_endpoint(&self) -> Option<WorkerEndpoint> {
        None
    }
}

/// Discovers gamescope's nested Xwayland cursor targets — `(DISPLAY, XAUTHORITY)`,
/// one per `--xwayland-count` — for [`Capturer::attach_gamescope_cursor`].
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
/// [`CursorChannelSender`]: the driver adopts the request's handle values iff the
/// IOCTL succeeds. Once per encoder generation.
#[cfg(target_os = "windows")]
pub type SetEncodeSender = std::sync::Arc<
    dyn Fn(
            &pf_driver_proto::encode::SetEncodeRequest,
        ) -> Result<pf_driver_proto::encode::SetEncodeReply>
        + Send
        + Sync,
>;

/// v7 one-shot encoder control (`IOCTL_ENCODE_CTL`): the `Encoder` calls the
/// stream loop makes, forwarded by the driver proxy.
#[cfg(target_os = "windows")]
pub type EncodeCtlSender =
    std::sync::Arc<dyn Fn(&pf_driver_proto::encode::EncodeCtlRequest) -> Result<()> + Send + Sync>;

/// Where an in-driver encoder is opened: the monitor's driver target and the WUDFHost the AU
/// section is duplicated into ([`Capturer::driver_endpoint`]).
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriverEndpoint {
    pub target_id: u32,
    pub wudf_pid: u32,
}
