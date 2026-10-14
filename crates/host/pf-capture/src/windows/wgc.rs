//! Host-side capture through a capture worker: a monitor the host did not create, captured
//! with Windows Graphics Capture and encoded in a process that runs as the signed-in user.
//!
//! [`WorkerLink`] is the host's end of one worker: its process handle, and the two pipes of
//! [`pf_driver_proto::worker`] behind a [`Link`], which checks and bounds everything the
//! worker sends. [`WgcCapturer`] is pixel-less like the IDD-push capturer — the worker owns
//! the pixels and the encoder, and publishes into the same AU section the driver does, so
//! [`open_worker_encoder`] hands the stream loop the same proxy.
//!
//! Evidence: `design/windows-wgc-capture.md` §4.3, §4.7, §4.13.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::windows::io::{AsRawHandle, BorrowedHandle, OwnedHandle};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use bytemuck::Pod;
use pf_driver_proto::encode as wire;
use pf_driver_proto::worker::{self as proto, kind};
use pf_encode_win::Encoder;
use windows::Win32::Foundation::{
    DuplicateHandle, DUPLICATE_CLOSE_SOURCE, DUPLICATE_HANDLE_OPTIONS, DUPLICATE_SAME_ACCESS,
    HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
};

use crate::idd_push::driver_encode::{open_remote_encoder, DriverEncodeParams, HandleTarget};
use crate::pixelless::{self, DeliveryGate};
use crate::worker_link::{Link, Message, Peer};
use crate::{CapturedFrame, Capturer, EncodeCtlSender, PixelFormat, SetEncodeSender};

/// How long the worker has to say hello. A process's first lines of `main`.
const HELLO_BOUND: Duration = Duration::from_secs(5);
/// How long a capture open may take. Its stages are calls that have hung on some box; a warm
/// open is 55 ms and a slow one 2.5 s.
const SOURCE_BOUND: Duration = Duration::from_secs(5);
/// An encoder open or reset: the worker's own 5 s bound on the backend, plus a thread stop.
const OPEN_BOUND: Duration = Duration::from_secs(8);
/// Every other op is queued in the worker and answered at once.
const CTL_BOUND: Duration = Duration::from_secs(2);
/// How long a worker gets to exit once its pipe closed, before it is ended.
const EXIT_BOUND: Duration = Duration::from_millis(500);
/// The largest picture a worker may report. Past it the reply is not a size.
const MAX_SIDE: u32 = 16_384;
/// A source with no new frame for this long reads as idle on the health surface.
const IDLE_AFTER: Duration = Duration::from_secs(2);

/// A started capture worker, as whoever spawned it hands it over.
pub struct WorkerProcess {
    /// The process, with the rights its creator holds: duplicate into, wait on, end.
    pub process: OwnedHandle,
    pub pid: u32,
    /// Host → worker.
    pub requests: File,
    /// Worker → host: replies and events.
    pub replies: File,
    /// The worker's stderr.
    pub log: File,
}

/// The worker's process, with the rights its creator holds: duplicate into, wait on, end.
struct Process(OwnedHandle);

impl Process {
    fn handle(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle())
    }
}

impl Peer for Process {
    fn kill(&self) {
        // SAFETY: the handle is live while `self` is, with the terminate right its creator
        // holds.
        let _ = unsafe { TerminateProcess(self.handle(), 1) };
    }

    fn exit_code(&self) -> Option<u32> {
        let mut code = 0u32;
        // SAFETY: the handle is live while `self` is, for both calls; `code` a live out-param.
        unsafe {
            (WaitForSingleObject(self.handle(), 0) == WAIT_OBJECT_0
                && GetExitCodeProcess(self.handle(), &mut code).is_ok())
            .then_some(code)
        }
    }
}

/// The host's end of one capture worker. Dropping the last reference closes the request pipe,
/// which is how a worker is told to exit; one that does not is ended.
pub struct WorkerLink {
    link: Link<Process, File>,
    pid: u32,
}

/// The worker's stderr into this process's log, a line at a time and a bounded line at that.
fn relay(log: File, pid: u32) {
    let mut log = BufReader::new(log);
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut log).take(4096).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let text = String::from_utf8_lossy(&line);
                tracing::info!(pid, "capture worker: {}", text.trim_end());
            }
        }
    }
}

impl WorkerLink {
    /// Take over a started worker: pump its pipes and trade hellos. A worker of another
    /// build is ended here.
    pub fn start(worker: WorkerProcess) -> Result<Arc<Self>> {
        let (log, pid) = (worker.log, worker.pid);
        let link = Link::open(Process(worker.process), worker.requests, worker.replies)?;
        std::thread::Builder::new()
            .name("pf-cw-log".into())
            .spawn(move || relay(log, pid))
            .context("spawn the capture worker log thread")?;
        let link = Arc::new(Self { link, pid });
        link.link.hello(HELLO_BOUND)?;
        Ok(link)
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    fn process(&self) -> HANDLE {
        self.link.peer().handle()
    }

    /// The worker's exit code once it has exited.
    pub fn exit_code(&self) -> Option<u32> {
        self.link.peer().exit_code()
    }

    /// End the worker now. It holds no state of ours: a session starts another.
    fn kill(&self) {
        self.link.peer().kill();
    }

    /// Raise the worker's GPU scheduling class from outside: the user's token cannot.
    fn raise_gpu_priority(&self) {
        // SAFETY: `process` is live for the call and carries the set-information right.
        unsafe { pf_frame::dxgi::elevate_gpu_priority_of(self.process(), "capture worker") };
    }

    /// [`Link::request`]: one request, its reply within `bound`, or the worker is ended.
    fn request<Q: Pod, R: Pod>(&self, op: u32, body: &Q, bound: Duration) -> Result<R> {
        self.link.request(op, body, bound)
    }

    /// The next event the worker sent, if any.
    fn event(&self) -> Option<Message> {
        self.link.event()
    }
}

impl Drop for WorkerLink {
    fn drop(&mut self) {
        self.link.close();
        // SAFETY: `process` is this link's live handle; a bounded wait only reads its state.
        let exited = unsafe {
            WaitForSingleObject(self.process(), EXIT_BOUND.as_millis() as u32) == WAIT_OBJECT_0
        };
        if !exited {
            tracing::warn!(pid = self.pid, "capture worker did not exit — ending it");
            self.kill();
        }
    }
}

impl HandleTarget for WorkerLink {
    fn dup_into(&self, h: BorrowedHandle<'_>, access: Option<u32>) -> Result<u64> {
        let mut out = HANDLE::default();
        let (desired, options) = match access {
            Some(rights) => (rights, DUPLICATE_HANDLE_OPTIONS(0)),
            None => (0, DUPLICATE_SAME_ACCESS),
        };
        // SAFETY: `h` is borrowed, so live; `process` is the live worker with the duplicate
        // right its creator holds; `&mut out` is a valid out-param.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                HANDLE(h.as_raw_handle()),
                self.process(),
                &mut out,
                desired,
                false,
                options,
            )
        }
        .context("DuplicateHandle into the capture worker")?;
        Ok(out.0 as usize as u64)
    }

    fn close_remote(&self, value: u64) {
        if value == 0 {
            return;
        }
        // SAFETY: `value` is a handle this link just created in the worker's table. Closing it
        // touches no other process, and fails harmlessly once the worker is gone.
        unsafe {
            let _ = DuplicateHandle(
                self.process(),
                HANDLE(value as usize as *mut core::ffi::c_void),
                HANDLE::default(),
                std::ptr::null_mut(),
                0,
                false,
                DUPLICATE_CLOSE_SOURCE,
            );
        }
    }
}

/// Where a worker session's encoder opens ([`Capturer::worker_endpoint`]): the worker, and the
/// monitor's display target for the request and the logs.
#[derive(Clone)]
pub struct WorkerEndpoint {
    pub link: Arc<WorkerLink>,
    pub target_id: u32,
}

/// Open the worker's encoder and hand back the stream loop's [`Encoder`]: the driver's proxy,
/// its two senders writing to the pipe instead of a device. The worker owns both duplicated
/// handles from the moment the request is written, whatever it answers.
pub fn open_worker_encoder(
    endpoint: &WorkerEndpoint,
    params: &DriverEncodeParams,
) -> Result<Box<dyn Encoder>> {
    let link = endpoint.link.clone();
    let set_encode: SetEncodeSender = Arc::new(move |req: &wire::SetEncodeRequest| {
        link.request(kind::SET_ENCODE, req, OPEN_BOUND)
    });
    let link = endpoint.link.clone();
    let encode_ctl: EncodeCtlSender = Arc::new(move |req: &wire::EncodeCtlRequest| {
        let bound = if req.op == wire::ENCODE_CTL_RESET {
            OPEN_BOUND
        } else {
            CTL_BOUND
        };
        let reply: proto::CtlReply = link.request(kind::ENCODE_CTL, req, bound)?;
        if reply.status != proto::CTL_OK {
            bail!(
                "capture worker refused control op {}: status {}",
                req.op,
                reply.status
            );
        }
        Ok(())
    });
    open_remote_encoder(
        &*endpoint.link,
        endpoint.target_id,
        params,
        set_encode,
        encode_ctl,
    )
}

/// The monitor a worker captures, as the host's display inventory names it now.
#[derive(Clone, Debug)]
pub struct WgcSource {
    /// GDI device name, the name the capture is opened by.
    pub gdi_name: String,
    /// The monitor's display target id.
    pub target_id: u32,
    /// The monitor composes HDR.
    pub hdr: bool,
}

fn side(v: u32) -> Option<u32> {
    (1..=MAX_SIDE).contains(&v).then_some(v)
}

fn stage_tag(stage: &[u8; 16]) -> String {
    let end = stage.iter().position(|&b| b == 0).unwrap_or(stage.len());
    String::from_utf8_lossy(&stage[..end]).into_owned()
}

/// Open `source` in `worker` and hand back its capturer. `want_hdr` is the session's: the
/// capture is FP16 only for an HDR session on a monitor that composes HDR. `fps` is the
/// stream's rate. `own_priority` gives the worker the GPU scheduling class a session's encoder
/// gets; a viewer of someone else's display leaves it off. `keepalive` is held for the
/// capturer's life.
pub fn open_wgc(
    worker: WorkerProcess,
    source: WgcSource,
    want_hdr: bool,
    fps: u32,
    own_priority: bool,
    keepalive: Box<dyn Send>,
) -> Result<Box<dyn Capturer>> {
    let link = WorkerLink::start(worker)?;
    if own_priority {
        link.raise_gpu_priority();
    }
    let fp16 = want_hdr && source.hdr;
    let gdi_name = proto::gdi_name(&source.gdi_name)
        .with_context(|| format!("monitor name {:?} is not a device name", source.gdi_name))?;
    let open = proto::OpenSource {
        flags: proto::OPEN_CURSOR | if fp16 { proto::OPEN_FP16 } else { 0 },
        frame_interval_100ns: 10_000_000 / fps.max(1),
        gdi_name,
    };
    let reply: proto::SourceReply = link.request(kind::OPEN_SOURCE, &open, SOURCE_BOUND)?;
    if reply.status != proto::SOURCE_OK {
        bail!(
            "open capture source {}: status {}, error {:#010x}, at '{}'",
            source.gdi_name,
            reply.status,
            reply.error as u32,
            stage_tag(&reply.stage)
        );
    }
    let asked = if fp16 {
        proto::FORMAT_FP16
    } else {
        proto::FORMAT_BGRA8
    };
    let (Some(width), Some(height), true) =
        (side(reply.width), side(reply.height), reply.format == asked)
    else {
        link.kill();
        bail!(
            "capture worker opened {} as {}x{} format {}",
            source.gdi_name,
            reply.width,
            reply.height,
            reply.format
        );
    };
    tracing::info!(
        gdi = %source.gdi_name,
        target_id = source.target_id,
        width,
        height,
        fp16,
        worker_pid = link.pid(),
        "capture worker: source open"
    );
    Ok(Box::new(WgcCapturer {
        link,
        source,
        width,
        height,
        fp16,
        encoder: None,
        delivery: DeliveryGate::new(),
        frames: 0,
        last_fresh: Instant::now(),
        _keepalive: keepalive,
    }))
}

/// The stream loop's [`Capturer`] for a worker session. A delivery carries no pixels: it is
/// the news that the worker's pool took a new frame, read off the encoder's own section.
struct WgcCapturer {
    link: Arc<WorkerLink>,
    source: WgcSource,
    width: u32,
    height: u32,
    /// The capture is FP16 scRGB and the stream HDR.
    fp16: bool,
    /// The session encoder's clocks, handed over once per loop tick.
    encoder: Option<pf_frame::health::EncoderTelemetry>,
    /// The freshness test, over the worker's source counter and the delivered size.
    delivery: DeliveryGate<(u32, u32)>,
    frames: u64,
    /// When the worker's pool last took a frame, for the health surface.
    last_fresh: Instant,
    _keepalive: Box<dyn Send>,
}

impl WgcCapturer {
    /// One tick: the worker's exit and events, then its frame cadence. `Ok(None)` means the
    /// desktop changed nothing since the last delivery.
    fn try_consume(&mut self) -> Result<Option<CapturedFrame>> {
        if let Some(code) = self.link.exit_code() {
            bail!("capture worker exited (code {code})");
        }
        while let Some((frame, body)) = self.link.event() {
            match frame.kind {
                kind::SOURCE_CHANGED => {
                    let changed = proto::body::<proto::SourceChanged>(&body)
                        .and_then(|c| Some((side(c.width)?, side(c.height)?)));
                    let Some((width, height)) = changed else {
                        bail!("capture worker reported a source change that is not a size");
                    };
                    tracing::info!(
                        gdi = %self.source.gdi_name,
                        from = format!("{}x{}", self.width, self.height),
                        to = format!("{width}x{height}"),
                        "capture worker: the source changed size"
                    );
                    (self.width, self.height) = (width, height);
                }
                kind::SOURCE_GONE => bail!("capture source {} is gone", self.source.gdi_name),
                other => tracing::debug!(kind = other, "capture worker: unknown event"),
            }
        }
        let seq = self.encoder.map(|t| t.source_seq);
        if !self.delivery.admit(seq, (self.width, self.height)) {
            return Ok(None);
        }
        self.frames += 1;
        self.last_fresh = Instant::now();
        let format = if self.fp16 {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        Ok(Some(pixelless::frame(
            self.frames,
            self.width,
            self.height,
            format,
        )))
    }
}

impl Capturer for WgcCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(f) = self.try_consume()? {
                return Ok(f);
            }
            if Instant::now() > deadline {
                bail!(
                    "no frame of {} within 20s — the capture worker's pool took none",
                    self.source.gdi_name
                );
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.try_consume()
    }

    fn is_alive(&self) -> bool {
        self.link.exit_code().is_none()
    }

    /// BT.2020 PQ while the capture is FP16, with the generic HDR10 volume the driver path
    /// sends.
    fn hdr_meta(&self) -> Option<pf_frame::HdrMeta> {
        self.fp16.then(pf_frame::hdr::generic_hdr10)
    }

    fn observe_encoder(&mut self, t: Option<pf_frame::health::EncoderTelemetry>) {
        self.encoder = t;
    }

    /// The operator surface's view. The capture delivers only what changes, so a quiet source
    /// is `idle`, never a stall, and no recovery rung hangs off this. Under the secure desktop
    /// the capture keeps showing the desktop without the prompt: that state is named.
    fn health(&self) -> Option<crate::CaptureHealth> {
        use pf_driver_proto::encode::au;
        let enc = self.encoder.as_ref();
        let source_gap = self.last_fresh.elapsed();
        let class = if pf_win_display::secure_desktop() {
            "secure_desktop"
        } else if source_gap > IDLE_AFTER {
            "idle"
        } else {
            "healthy"
        };
        Some(crate::CaptureHealth {
            class,
            stall_class: None,
            source_gap,
            evidence: None,
            present_to_arrival: enc.and_then(|e| e.present_to_arrival),
            late_frames: false,
            encoder_state: enc.map(|e| match e.state {
                au::ENCODER_CLOSED => "closed",
                au::ENCODER_OPEN => "open",
                au::ENCODER_ENCODING => "encoding",
                _ => "wedged",
            }),
            backend_opened: enc.map(|e| e.backend),
            detached: 0,
            published_total: enc.map_or(0, |e| e.published_total),
            dropped_total: enc.map_or(0, |e| e.dropped_total),
            source_seq: enc.map_or(0, |e| e.source_seq),
            current_stage: None,
            last_episode: None,
            episodes_suppressed: 0,
            cooldown_remaining: None,
        })
    }

    fn worker_endpoint(&self) -> Option<WorkerEndpoint> {
        Some(WorkerEndpoint {
            link: self.link.clone(),
            target_id: self.source.target_id,
        })
    }
}
