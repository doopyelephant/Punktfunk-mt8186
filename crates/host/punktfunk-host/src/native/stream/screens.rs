//! Screens past the first: a display, capture and encoder each, on a video stream of its own.
//!
//! The first screen is [`StreamState`](super::state::StreamState)'s whole loop: recovery,
//! pacing, cursor forwarding, the game. Each further screen is one thread running the session's
//! plan with the pointer drawn into its picture, and sending whole access units into the
//! session's one send thread under its own stream id, epoch and frame index. It keyframes on the
//! client's ask, rebuilds on a lost capture and encodes at its share of the session's budget.
//! [`ScreenSet`] follows the client's `Screens` and tells it what was built.

use super::pipeline::{build_pipeline_with_retry, BuildRequest, Pipeline};
use super::send::{AuMeta, FrameMsg, SendMsg};
use super::*;
use crate::native::bitrate::{EncDerive, SHARE_ALL};
use crate::native::wiring::{ScreenTell, ScreenWire};
use punktfunk_core::quic::v2::msg::{Screen, StreamConfig, SCREENS_MAX};
use std::sync::atomic::AtomicU16;

/// A keyframe ask inside this long of the last keyframe is the same loss asked again.
const KEY_EVERY: std::time::Duration = std::time::Duration::from_millis(100);
/// Builds of further screens start this far apart and after the first screen's: a compositor
/// that adds two outputs at once can fail both (Mutter's monitor rebuild).
const BUILD_SPACING: std::time::Duration = std::time::Duration::from_millis(1500);
/// Lost captures in a row before a screen gives up and leaves the layout.
const MAX_REBUILDS: u32 = 4;

/// The first screen, as the further screens place themselves against it.
#[derive(Clone, Copy, Default)]
pub(super) struct FirstScreen {
    /// The client's screen 0 record: its monitors sit relative to this one.
    asked: Screen,
    /// What the first screen delivers now, and its display's registry generation.
    mode: punktfunk_core::Mode,
    display_gen: Option<u64>,
}

/// Where the client's screen `ask` sits from its first, scaled from the client's pixels to what
/// the host delivers on the first. `None` when the client named no offset.
fn offset_of(first: &FirstScreen, ask: &Screen) -> Option<(i32, i32)> {
    if (ask.x, ask.y) == (first.asked.x, first.asked.y) {
        return None;
    }
    let scale = |d: i32, host: u32, client: u32| {
        (i64::from(d) * i64::from(host.max(1)) / i64::from(client.max(1))) as i32
    };
    Some((
        scale(
            ask.x - first.asked.x,
            first.mode.width,
            first.asked.mode.width,
        ),
        scale(
            ask.y - first.asked.y,
            first.mode.height,
            first.asked.mode.height,
        ),
    ))
}

/// Put screen `run`'s display beside the first as the client's monitors sit, and say where it
/// is. The manager holds the place through every later arrangement.
#[cfg(target_os = "windows")]
fn place(
    _vd: &mut dyn crate::vdisplay::VirtualDisplay,
    fp: [u8; 32],
    base: &ScreenBase,
    run: &ScreenRun,
    _display_gen: Option<u64>,
) -> (i32, i32) {
    let first = *base.first.lock().unwrap_or_else(|e| e.into_inner());
    crate::vdisplay::manager::place_beside(fp, base.params.client_fp, offset_of(&first, &run.ask))
        .unwrap_or((0, 0))
}

/// [`place`] by the backend's own move; one that cannot move an output keeps the compositor's.
#[cfg(not(target_os = "windows"))]
fn place(
    vd: &mut dyn crate::vdisplay::VirtualDisplay,
    _fp: [u8; 32],
    base: &ScreenBase,
    run: &ScreenRun,
    display_gen: Option<u64>,
) -> (i32, i32) {
    let first = *base.first.lock().unwrap_or_else(|e| e.into_inner());
    let at = offset_of(&first, &run.ask)
        .zip(position_of(first.display_gen))
        .map(|((dx, dy), (x, y))| (x + dx, y + dy));
    if let Some((x, y)) = at {
        vd.apply_position(x, y);
    }
    at.or_else(|| position_of(display_gen)).unwrap_or((0, 0))
}

/// What every screen loop of a session starts from.
#[derive(Clone)]
pub(super) struct ScreenBase {
    pub(super) compositor: crate::vdisplay::Compositor,
    /// The first screen's display request; each screen swaps in its own identity.
    pub(super) params: crate::vdisplay::SessionParams,
    pub(super) plan: crate::session_plan::SessionPlan,
    /// The session's encode resolution, which every screen opens on.
    pub(super) encoder: Arc<crate::encode::Resolved>,
    pub(super) bit_depth: u8,
    pub(super) enc_of: EncDerive,
    pub(super) fec_target: Arc<AtomicU8>,
    /// The session's wire budget, kbps: every screen encodes at its share of it.
    pub(super) live_bitrate: Arc<AtomicU32>,
    pub(super) send: Arc<std::sync::mpsc::SyncSender<SendMsg>>,
    pub(super) tell: tokio::sync::mpsc::UnboundedSender<ScreenTell>,
    pub(super) wire: Arc<ScreenWire>,
    pub(super) life: crate::lifecycle::Lifecycle,
    /// Kept current by [`ScreenSet::poll`].
    pub(super) first: Arc<std::sync::Mutex<FirstScreen>>,
}

/// One further screen's loop, as the set holds it.
struct ScreenHandle {
    ask: Screen,
    share: Arc<AtomicU16>,
    stop: Arc<AtomicBool>,
    /// What the loop built; `None` before it has, and after it gave up.
    built: Arc<std::sync::Mutex<Option<Screen>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ScreenHandle {
    fn built(&self) -> Option<Screen> {
        *self.built.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The screens past the first a session runs, following the client's `Screens`.
pub(super) struct ScreenSet {
    /// `None` once [`Self::shutdown`] ran: the send thread must not outlive its last sender.
    base: Option<ScreenBase>,
    rx: std::sync::mpsc::Receiver<Vec<Screen>>,
    loops: Vec<ScreenHandle>,
    /// The client's screen 0 record, for its position and primary flag.
    first: Screen,
    /// The layout last told.
    told: Vec<Screen>,
    /// When the next build may start ([`BUILD_SPACING`]).
    next_build: std::time::Instant,
}

impl ScreenSet {
    /// The set for a session that negotiated screens, building those `asked` names past the first.
    pub(super) fn new(
        base: ScreenBase,
        rx: std::sync::mpsc::Receiver<Vec<Screen>>,
        asked: Vec<Screen>,
    ) -> Self {
        let first = asked
            .iter()
            .find(|s| s.index == 0)
            .copied()
            .unwrap_or_default();
        let mut set = ScreenSet {
            base: Some(base),
            rx,
            loops: Vec::new(),
            first,
            told: Vec::new(),
            next_build: std::time::Instant::now() + BUILD_SPACING,
        };
        set.apply(asked);
        set
    }

    /// Each turn of the first screen's loop: the client's newest list, finished loops, the
    /// layout when it moved. `first_mode` is what the first screen delivers now, on the display
    /// of registry generation `first_gen`.
    pub(super) fn poll(&mut self, first_mode: punktfunk_core::Mode, first_gen: Option<u64>) {
        if let Some(base) = self.base.as_ref() {
            let mut f = base.first.lock().unwrap_or_else(|e| e.into_inner());
            (f.asked, f.mode, f.display_gen) = (self.first, first_mode, first_gen);
        }
        let mut newest = None;
        while let Ok(list) = self.rx.try_recv() {
            newest = Some(list);
        }
        if let Some(list) = newest {
            self.apply(list);
        }
        let before = self.loops.len();
        self.loops.retain_mut(|h| {
            let done = h.thread.as_ref().is_some_and(|t| t.is_finished());
            if done {
                h.stop_and_join();
            }
            !done
        });
        if self.loops.len() != before {
            self.reshare(first_mode);
        }
        self.tell_layout(first_mode);
    }

    /// Run exactly the screens `list` names past the first: stop the ones it drops or resizes,
    /// start the ones it adds.
    fn apply(&mut self, list: Vec<Screen>) {
        let Some(base) = self.base.clone() else {
            return;
        };
        if let Some(first) = list.iter().find(|s| s.index == 0) {
            self.first = *first;
        }
        let mut want: Vec<Screen> = Vec::new();
        for s in list {
            if screen_fits(&s, base.plan.codec) && !want.iter().any(|w| w.index == s.index) {
                want.push(s);
            }
        }
        self.loops.retain_mut(|h| {
            let kept = want
                .iter()
                .any(|w| w.index == h.ask.index && w.mode == h.ask.mode);
            if !kept {
                h.stop_and_join();
            }
            kept
        });
        for w in want {
            if self.loops.iter().any(|h| h.ask.index == w.index) {
                continue;
            }
            let not_before = self.next_build.max(std::time::Instant::now());
            self.next_build = not_before + BUILD_SPACING;
            let handle = ScreenHandle {
                ask: w,
                share: Arc::new(AtomicU16::new(SHARE_ALL / 2)),
                stop: Arc::new(AtomicBool::new(false)),
                built: Arc::default(),
                thread: None,
            };
            let run = ScreenRun {
                index: w.index,
                stream: punktfunk_core::packet::screen_stream(w.index),
                ask: w,
                mode: w.mode,
                share: handle.share.clone(),
                stop: handle.stop.clone(),
                built: handle.built.clone(),
                not_before,
            };
            let base = base.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("punktfunk-screen-{}", w.index))
                .spawn(move || run_screen(base, run));
            match spawned {
                Ok(t) => self.loops.push(ScreenHandle {
                    thread: Some(t),
                    ..handle
                }),
                Err(e) => {
                    tracing::warn!(screen = w.index, error = %e, "screen thread did not spawn")
                }
            }
        }
        let first_mode = self.told.first().map_or(self.first.mode, |s| s.mode);
        self.reshare(first_mode);
    }

    /// Split the video by pixel rate: the first screen's share to the first loop, each further
    /// screen's to its own.
    fn reshare(&self, first_mode: punktfunk_core::Mode) {
        let Some(base) = self.base.as_ref() else {
            return;
        };
        let asks: Vec<punktfunk_core::Mode> = self.loops.iter().map(|h| h.ask.mode).collect();
        let (first, rest) = shares(first_mode, &asks);
        base.wire.primary_share.store(first, Ordering::Relaxed);
        for (h, s) in self.loops.iter().zip(rest) {
            h.share.store(s, Ordering::Relaxed);
        }
    }

    /// Tell the client every screen built, when that changed.
    fn tell_layout(&mut self, first_mode: punktfunk_core::Mode) {
        let Some(base) = self.base.as_ref() else {
            return;
        };
        let mut layout = vec![Screen {
            index: 0,
            stream: punktfunk_core::packet::V2_STREAM_VIDEO,
            mode: first_mode,
            ..self.first
        }];
        layout.extend(self.loops.iter().filter_map(ScreenHandle::built));
        if layout != self.told {
            self.told = layout.clone();
            let _ = base.tell.send(ScreenTell::Layout(layout));
        }
    }

    /// Stop every loop and drop the set's sender, before the send thread is joined.
    pub(super) fn shutdown(&mut self) {
        for h in &self.loops {
            h.stop.store(true, Ordering::SeqCst);
        }
        for h in &mut self.loops {
            h.stop_and_join();
        }
        self.loops.clear();
        self.base = None;
    }
}

impl Drop for ScreenSet {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The shares of the video, per mille, by pixel rate: the first screen's, then each of `rest`'s.
fn shares(first: punktfunk_core::Mode, rest: &[punktfunk_core::Mode]) -> (u16, Vec<u16>) {
    let rate = |m: &punktfunk_core::Mode| {
        u64::from(m.width) * u64::from(m.height) * u64::from(m.refresh_hz.max(1))
    };
    let total = rate(&first) + rest.iter().map(rate).sum::<u64>();
    let share = |m| (rate(m) * u64::from(SHARE_ALL) / total.max(1)).max(1) as u16;
    if rest.is_empty() {
        return (SHARE_ALL, Vec::new());
    }
    (share(&first), rest.iter().map(share).collect())
}

/// Screen `index`'s display identity: its own slot, output name and scale memory, apart from the
/// first screen's, the same for this client every session.
fn screen_identity(fp: Option<[u8; 32]>, index: u8) -> [u8; 32] {
    use sha2::Digest as _;
    let mut seed = b"punktfunk screen".to_vec();
    seed.extend_from_slice(&fp.unwrap_or_default());
    seed.push(index);
    sha2::Sha256::digest(&seed).into()
}

/// One loop's own values.
struct ScreenRun {
    index: u8,
    stream: u8,
    /// The client's record of this screen: its size and where it sits.
    ask: Screen,
    mode: punktfunk_core::Mode,
    share: Arc<AtomicU16>,
    stop: Arc<AtomicBool>,
    built: Arc<std::sync::Mutex<Option<Screen>>>,
    not_before: std::time::Instant,
}

impl ScreenRun {
    fn leaving(&self, base: &ScreenBase) -> bool {
        self.stop.load(Ordering::SeqCst) || base.life.stopping()
    }

    fn set_built(&self, built: Option<Screen>) {
        *self.built.lock().unwrap_or_else(|e| e.into_inner()) = built;
    }
}

/// How one pipeline's loop ended.
enum Ended {
    Stop,
    Rebuild,
}

/// A further screen from first build to stop: build, stream, rebuild on a lost capture.
fn run_screen(base: ScreenBase, run: ScreenRun) {
    let _span = tracing::info_span!("screen", index = run.index).entered();
    while std::time::Instant::now() < run.not_before {
        if run.leaving(&base) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let fp = screen_identity(base.params.client_fp, run.index);
    let mut params = base.params.clone();
    params.client_fp = Some(fp);
    // The compositor draws the pointer into this picture; the client's cursor is the first's.
    params.hw_cursor = false;
    params.launch = None;
    params.join_live = false;
    let plan = crate::session_plan::SessionPlan {
        cursor_forward: false,
        cursor_blend: false,
        gamescope_cursor: false,
        reframe_to: None,
        ..base.plan
    };
    if let Some(why) = over_budget() {
        tracing::warn!(screen = run.index, "{why} — the screen is left out");
        return;
    }
    let mut vd = match crate::vdisplay::open(base.compositor) {
        Ok(vd) => vd,
        Err(e) => {
            tracing::warn!(screen = run.index, error = %format!("{e:#}"), "screen display backend did not open");
            return;
        }
    };
    params.apply(&mut *vd);
    let quit = base.life.quit_flag().clone();
    let (mut epoch, mut au_seq, mut lost) = (0u8, 0u32, 0u32);
    while !run.leaving(&base) && lost <= MAX_REBUILDS {
        let pipe = {
            let _idd_setup = match crate::windows::idd::setup_guard(
                plan.capture,
                Some(fp),
                (run.mode.width, run.mode.height),
                &run.stop,
            ) {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!(screen = run.index, error = %format!("{e:#}"), "screen slot refused");
                    break;
                }
            };
            let req = BuildRequest {
                mode: run.mode,
                bitrate_kbps: base.live_bitrate.load(Ordering::Relaxed),
                bitrate_auto: false,
                bit_depth: base.bit_depth,
                enc_of: enc_of(&base, &run),
                plan,
                encoder: base.encoder.clone(),
                client_hdr: base.params.client_hdr,
                wire_seq_base: au_seq,
                screen: run.index,
            };
            build_pipeline_with_retry(&mut vd, &req, &quit, &run.stop, None, 4, true, None)
        };
        let pipe = match pipe {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(screen = run.index, error = %format!("{e:#}"), "screen did not build");
                break;
            }
        };
        let delivered = delivered_mode(pipe.frame.width, pipe.frame.height, pipe.interval);
        let mut cfg = StreamConfig {
            stream: run.stream,
            epoch,
            mode: delivered,
            ..Default::default()
        };
        cfg.set_format(super::state::format_of(
            &plan,
            &*pipe.capturer,
            base.bit_depth,
        ));
        let _ = base.tell.send(ScreenTell::Config(cfg));
        let (x, y) = place(&mut *vd, fp, &base, &run, pipe.display_gen);
        tracing::info!(screen = run.index, ?delivered, x, y, "screen built");
        run.set_built(Some(Screen {
            index: run.index,
            stream: run.stream,
            mode: delivered,
            x,
            y,
            primary: false,
        }));
        let sent_before = au_seq;
        match stream_screen(&base, &run, pipe, epoch, &mut au_seq) {
            Ended::Stop => break,
            Ended::Rebuild => {
                lost = losses_in_a_row(lost, au_seq != sent_before);
                epoch = epoch.wrapping_add(1);
            }
        }
    }
    if lost > MAX_REBUILDS {
        tracing::warn!(
            screen = run.index,
            lost,
            "screen's capture was lost again after each rebuild — the screen is left out"
        );
    }
    run.set_built(None);
}

/// A further screen the client may ask for: a slot past the first, at a mode the handshake
/// and a Reconfigure would accept for the first screen. The size reaches the compositor as a
/// virtual output before any encoder can refuse it.
fn screen_fits(s: &Screen, codec: crate::encode::Codec) -> bool {
    s.index > 0
        && usize::from(s.index) < SCREENS_MAX
        && crate::encode::validate_dimensions(codec, s.mode.width, s.mode.height).is_ok()
        && crate::encode::validate_refresh(s.mode.refresh_hz).is_ok()
}

/// The loss count after a capture loss: a pipeline that `delivered` a frame was a recovery,
/// so this loss starts a new row.
fn losses_in_a_row(lost: u32, delivered: bool) -> u32 {
    if delivered {
        1
    } else {
        lost + 1
    }
}

/// Over the host's `max_displays` with this screen, live and pinned displays counted.
fn over_budget() -> Option<String> {
    let max = crate::vdisplay::policy::prefs()
        .get()
        .effective()
        .max_displays;
    let held = crate::vdisplay::registry::snapshot()
        .displays
        .iter()
        .filter(|d| d.state != "lingering")
        .count() as u32;
    (held >= max)
        .then(|| format!("host display budget exhausted: {held} live, max_displays = {max}"))
}

/// A display's desktop origin, from the registry the console reads.
#[cfg(not(target_os = "windows"))]
fn position_of(display_gen: Option<u64>) -> Option<(i32, i32)> {
    let generation = display_gen?;
    crate::vdisplay::registry::snapshot()
        .displays
        .iter()
        .find(|d| d.slot == generation)
        .map(|d| d.position)
}

/// The rate derivation at this screen's share and the FEC in force.
fn enc_of(base: &ScreenBase, run: &ScreenRun) -> EncDerive {
    EncDerive {
        fec_percent: base.fec_target.load(Ordering::Relaxed),
        share_permille: run.share.load(Ordering::Relaxed),
        ..base.enc_of
    }
}

/// One pipeline's frames, at its own cadence, until stopped or its capture is lost.
fn stream_screen(
    base: &ScreenBase,
    run: &ScreenRun,
    pipe: Pipeline,
    epoch: u8,
    au_seq: &mut u32,
) -> Ended {
    let Pipeline {
        mut capturer,
        mut enc,
        mut frame,
        interval,
        ..
    } = pipe;
    // Capture times of the frames handed to the encoder and not yet polled.
    let mut inflight = std::collections::VecDeque::<u64>::new();
    let mut rate = (base.live_bitrate.load(Ordering::Relaxed), enc_of(base, run));
    let mut last_key = std::time::Instant::now();
    let mut next = std::time::Instant::now();
    loop {
        if run.leaving(base) {
            return Ended::Stop;
        }
        if base.wire.take_keyframe(run.stream) && last_key.elapsed() >= KEY_EVERY {
            enc.request_keyframe();
            last_key = std::time::Instant::now();
        }
        let now_rate = (base.live_bitrate.load(Ordering::Relaxed), enc_of(base, run));
        if (
            now_rate.0,
            now_rate.1.share_permille,
            now_rate.1.fec_percent,
        ) != (rate.0, rate.1.share_permille, rate.1.fec_percent)
        {
            rate = now_rate;
            enc.reconfigure_bitrate(u64::from(rate.1.enc_kbps(rate.0)) * 1000);
        }
        if let Some(feed) = capturer.feed() {
            feed.wait_arrival(next);
        } else if let Some(wait) = next.checked_duration_since(std::time::Instant::now()) {
            std::thread::sleep(wait);
        }
        let fresh = match capturer.try_latest() {
            Ok(Some(f)) => {
                frame = f;
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(screen = run.index, error = %format!("{e:#}"), "screen capture lost — rebuilding");
                return Ended::Rebuild;
            }
        };
        let now = now_ns();
        let capture_ns = if fresh && frame.pts_ns > 0 && frame.pts_ns <= now {
            frame.pts_ns
        } else {
            now
        };
        let owed =
            crate::send_pacing::owed_aus(&frame, &mut *enc, std::time::Instant::now() + interval);
        if owed.is_none() {
            let wire_index = au_seq.wrapping_add(inflight.len() as u32);
            if let Err(e) = enc.submit_indexed(&frame, wire_index) {
                tracing::warn!(screen = run.index, error = %format!("{e:#}"), "screen encode failed — rebuilding");
                return Ended::Rebuild;
            }
        }
        for _ in 0..owed.unwrap_or(1) {
            inflight.push_back(capture_ns);
        }
        while !inflight.is_empty() {
            let au = match enc.poll() {
                Ok(Some(au)) => au,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(screen = run.index, error = %format!("{e:#}"), "screen encoder failed — rebuilding");
                    return Ended::Rebuild;
                }
            };
            let cap = inflight.pop_front().unwrap_or(now);
            if au.keyframe {
                last_key = std::time::Instant::now();
            }
            let msg = FrameMsg {
                meta: AuMeta {
                    capture_ns: if owed.is_some() && au.pts_ns > 0 {
                        au.pts_ns
                    } else {
                        cap
                    },
                    flags: if au.keyframe {
                        (FLAG_PIC | FLAG_SOF) as u32
                    } else {
                        FLAG_PIC as u32
                    },
                    epoch,
                    stream: run.stream,
                    frame_index: *au_seq,
                    encode_us: 0,
                    queue_us: 0,
                    cap_us: 0,
                    submit_us: 0,
                    wait_us: 0,
                    repeat: !fresh,
                    was_measured: false,
                    driver: None,
                },
                data: au.data,
            };
            if !hand_to_send(base, run, msg) {
                return Ended::Stop;
            }
            *au_seq = au_seq.wrapping_add(1);
        }
        next += interval;
        let now = std::time::Instant::now();
        if next < now {
            next = now;
        }
    }
}

/// Queue `msg` on the send thread, waiting while its queue is full. `false` once the thread is
/// gone or this loop is leaving.
fn hand_to_send(base: &ScreenBase, run: &ScreenRun, msg: FrameMsg) -> bool {
    let mut msg = SendMsg::Frame(msg);
    loop {
        match base.send.try_send(msg) {
            Ok(()) => return true,
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return false,
            Err(std::sync::mpsc::TrySendError::Full(back)) => {
                if run.leaving(base) {
                    return false;
                }
                msg = back;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A further screen is held to the first screen's mode rules before any display is made.
    #[test]
    fn a_further_screen_asks_a_mode_the_first_would_get() {
        let screen = |index, width, height, refresh_hz| Screen {
            index,
            mode: punktfunk_core::Mode {
                width,
                height,
                refresh_hz,
            },
            ..Default::default()
        };
        let hevc = crate::encode::Codec::H265;
        assert!(screen_fits(&screen(1, 1920, 1080, 60), hevc));
        assert!(
            !screen_fits(&screen(0, 1920, 1080, 60), hevc),
            "the first screen"
        );
        assert!(!screen_fits(&screen(1, 65_535, 65_535, 60), hevc));
        assert!(!screen_fits(&screen(1, 1921, 1080, 60), hevc), "odd width");
        assert!(!screen_fits(&screen(1, 1920, 0, 60), hevc));
        assert!(!screen_fits(&screen(1, 1920, 1080, 0), hevc));
    }

    /// A screen that streamed between its losses keeps healing; only losses with no frame
    /// between them run out the count.
    #[test]
    fn a_screen_gives_up_only_on_losses_in_a_row() {
        let mut lost = 0;
        for _ in 0..3 * MAX_REBUILDS {
            lost = losses_in_a_row(lost, true);
        }
        assert_eq!(lost, 1, "recovered losses were counted against the screen");
        for _ in 0..MAX_REBUILDS {
            lost = losses_in_a_row(lost, false);
        }
        assert!(lost > MAX_REBUILDS);
    }

    fn mode(width: u32, height: u32, refresh_hz: u32) -> punktfunk_core::Mode {
        punktfunk_core::Mode {
            width,
            height,
            refresh_hz,
        }
    }

    /// One screen takes the whole video; two take it by pixel rate.
    #[test]
    fn screens_share_the_video_by_pixel_rate() {
        assert_eq!(shares(mode(1920, 1080, 60), &[]), (SHARE_ALL, vec![]));
        let (first, rest) = shares(mode(1920, 1080, 60), &[mode(1920, 1080, 60)]);
        assert_eq!((first, rest), (500, vec![500]));
        let (first, rest) = shares(mode(3840, 2160, 60), &[mode(1920, 1080, 60)]);
        assert_eq!((first, rest), (800, vec![200]));
        let (first, rest) = shares(mode(2560, 1440, 144), &[mode(1920, 1080, 60)]);
        assert!(first + rest[0] <= SHARE_ALL && first > rest[0] * 3);
    }

    /// A KWin session's base for a set under test, with the ends its frames and layouts reach.
    /// Hold the frame end for the test's length.
    fn kwin_base() -> (
        ScreenBase,
        std::sync::mpsc::Receiver<SendMsg>,
        tokio::sync::mpsc::UnboundedReceiver<ScreenTell>,
        Arc<ScreenWire>,
    ) {
        let (send, frames) = std::sync::mpsc::sync_channel(3);
        let (tell, told) = tokio::sync::mpsc::unbounded_channel();
        let wire = Arc::new(ScreenWire::new());
        let base = ScreenBase {
            compositor: crate::vdisplay::Compositor::Kwin,
            params: crate::vdisplay::SessionParams {
                client_fp: Some([3; 32]),
                client_hdr: None,
                hdr: false,
                hw_cursor: true,
                join_live: false,
                quit: Arc::default(),
                launch: Some("steam:570".into()),
                route: None,
                isolation: None,
            },
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
                cursor_forward: true,
                gamescope_cursor: false,
                sdr10_native: false,
                slices: 1,
                reframe_to: None,
            },
            encoder: crate::encode::resolve(),
            bit_depth: 8,
            enc_of: EncDerive {
                audio_kbps: 512,
                shard_payload: 1408,
                fec_percent: 10,
                head_only_hz: None,
                share_permille: SHARE_ALL,
            },
            fec_target: Arc::new(AtomicU8::new(10)),
            live_bitrate: Arc::new(AtomicU32::new(40_000)),
            send: Arc::new(send),
            tell,
            wire: wire.clone(),
            life: crate::lifecycle::Lifecycle::new(),
            first: Arc::default(),
        };
        (base, frames, told, wire)
    }

    fn asked(index: u8, mode: punktfunk_core::Mode) -> Screen {
        Screen {
            index,
            mode,
            ..Default::default()
        }
    }

    /// A set asked for a second screen splits the video and tells a layout of what is built,
    /// screen 0 alone until the second builds; dropping the second stops its loop and hands the
    /// whole video back.
    #[test]
    fn the_set_follows_the_clients_screens() {
        let (base, _frames, mut told, wire) = kwin_base();
        let (screens_tx, screens_rx) = std::sync::mpsc::channel();
        let one = mode(1920, 1080, 60);
        let mut set = ScreenSet::new(base, screens_rx, vec![asked(0, one), asked(1, one)]);
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), 500);
        set.poll(one, None);
        let Ok(ScreenTell::Layout(layout)) = told.try_recv() else {
            panic!("a layout is told at once");
        };
        assert_eq!(layout.len(), 1, "only screen 0 is built yet");
        assert_eq!(
            (layout[0].index, layout[0].stream, layout[0].mode),
            (0, 0, one)
        );
        screens_tx.send(vec![asked(0, one)]).unwrap();
        set.poll(one, None);
        assert!(set.loops.is_empty(), "screen 1's loop stopped");
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), SHARE_ALL);
        assert!(told.try_recv().is_err(), "the layout did not change");
        set.shutdown();
        assert!(set.base.is_none());
    }

    /// A client that promised screens but asked for none at Hello: screen 0 is told once and
    /// then nothing, and a screen asked for later starts as one asked at Hello.
    #[test]
    fn a_set_with_no_ask_waits_quietly() {
        let (base, _frames, mut told, wire) = kwin_base();
        let (screens_tx, screens_rx) = std::sync::mpsc::channel();
        let one = mode(1920, 1080, 60);
        let mut set = ScreenSet::new(base, screens_rx, Vec::new());
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), SHARE_ALL);
        set.poll(one, None);
        let Ok(ScreenTell::Layout(layout)) = told.try_recv() else {
            panic!("screen 0 is told at once");
        };
        assert_eq!((layout.len(), layout[0].index, layout[0].mode), (1, 0, one));
        set.poll(one, None);
        assert!(told.try_recv().is_err(), "nothing moved, nothing told");
        screens_tx.send(vec![asked(0, one), asked(1, one)]).unwrap();
        set.poll(one, None);
        assert_eq!(set.loops.len(), 1, "screen 1's loop started");
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), 500);
        set.shutdown();
    }

    /// A screen sits where the client's monitor does beside its first, scaled to what the host
    /// delivers on the first; one the client placed on top of the first is left to the host.
    #[test]
    fn a_screen_sits_where_the_clients_monitor_does() {
        let first = FirstScreen {
            asked: Screen {
                mode: mode(2560, 1440, 60),
                ..Default::default()
            },
            mode: mode(1920, 1080, 60),
            display_gen: Some(1),
        };
        let left = Screen {
            index: 1,
            mode: mode(1920, 1080, 60),
            x: -1920,
            y: 240,
            ..Default::default()
        };
        assert_eq!(offset_of(&first, &left), Some((-1440, 180)));
        let unplaced = Screen { x: 0, y: 0, ..left };
        assert_eq!(offset_of(&first, &unplaced), None);
    }

    /// Each screen of a client has one identity, apart from its other screens' and from
    /// another client's.
    #[test]
    fn a_screen_keeps_its_own_identity() {
        let fp = Some([7u8; 32]);
        assert_eq!(screen_identity(fp, 1), screen_identity(fp, 1));
        assert_ne!(screen_identity(fp, 1), screen_identity(fp, 2));
        assert_ne!(screen_identity(fp, 1), screen_identity(Some([8u8; 32]), 1));
        assert_ne!(screen_identity(fp, 1), [7u8; 32]);
    }
}
