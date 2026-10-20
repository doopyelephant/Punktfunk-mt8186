//! Frame-capture facade over `pf-capture`.
//!
//! Re-exports the shared frame types and capturer traits at the historical
//! `crate::capture::*` paths. Host-only entry points — [`open_portal_monitor`],
//! [`capture_virtual_output`] — resolve [`pf_capture::ZeroCopyPolicy`] and, on
//! Windows, the driver-IOCTL senders so the capturer never reaches back into
//! encode or vdisplay.

use anyhow::Result;

pub use pf_frame::{CapturedFrame, OutputFormat};
// Named only by the GameStream media path and the Linux pyrowave plumbing below.
// Off both, a native-only Windows build would trip -D warnings.
#[cfg(any(target_os = "linux", feature = "gamestream"))]
pub use pf_frame::PixelFormat;
// `capturer_supports_hdr` is not re-exported: on Linux that name is the platform
// floor and would silently miss the gamescope arm. Use [`capturer_supports_hdr_for`].
#[cfg(feature = "gamestream")]
pub use pf_capture::FastSyntheticCapturer;
pub use pf_capture::{capturer_supports_444, Capturer, SyntheticCapturer};
#[cfg(target_os = "windows")]
pub use pf_capture::{gpu_pref, synthetic_nv12};

/// What the session's encoder proved it can import, per capture fourcc. SDR
/// asks the packed-RGB pair, HDR the packed 10-bit pair; empty verdicts carry
/// no entry, so an unproved fourcc stays LINEAR-only. `None` (portal/diagnostic
/// opens) advertises no encoder-proved tiled modifiers.
#[cfg(target_os = "linux")]
fn encoder_modifiers(
    codec: Option<crate::encode::Codec>,
    bit_depth: u8,
    hdr: bool,
) -> Vec<(u32, Vec<u64>)> {
    codec.map_or_else(Vec::new, |codec| {
        let formats = if hdr {
            [PixelFormat::X2Bgr10, PixelFormat::X2Rgb10]
        } else {
            [PixelFormat::Bgrx, PixelFormat::Bgra]
        };
        formats
            .into_iter()
            .filter_map(|fmt| {
                let fourcc = pf_frame::drm_fourcc(fmt)?;
                let mods = crate::encode::linux_capture_modifiers(codec, fourcc, bit_depth, hdr);
                (!mods.is_empty()).then_some((fourcc, mods))
            })
            .collect()
    })
}

/// Encode route and per-fourcc import verdicts for one Linux capture session.
/// Resolved here so pf-capture never reaches back into encode.
#[cfg(target_os = "linux")]
fn zero_copy_policy(
    pyrowave_session: bool,
    native_nv12_session: bool,
    codec: Option<crate::encode::Codec>,
    bit_depth: u8,
    hdr: bool,
) -> pf_capture::ZeroCopyPolicy {
    let backend_is_vaapi = crate::encode::linux_zero_copy_is_vaapi();
    // Raw-dmabuf passthrough serves PyroWave on any vendor: the wavelet encoder
    // imports the dmabuf on its own Vulkan device. The `PUNKTFUNK_ENCODER=pyrowave`
    // lab lever also flips `backend_is_vaapi`.
    #[cfg(feature = "pyrowave")]
    let pyrowave_session =
        pyrowave_session || pf_host_config::config().encoder_pref.as_str() == "pyrowave";
    #[cfg(not(feature = "pyrowave"))]
    let pyrowave_session = {
        let _ = pyrowave_session;
        false
    };
    let modifier_codec = if pyrowave_session {
        Some(crate::encode::Codec::PyroWave)
    } else {
        codec
    };
    let encoder_modifiers = if backend_is_vaapi || pyrowave_session {
        encoder_modifiers(modifier_codec, bit_depth, hdr)
    } else {
        Vec::new()
    };
    pf_capture::ZeroCopyPolicy {
        backend_is_vaapi,
        backend_is_gpu: crate::encode::resolved_backend_is_gpu(),
        pyrowave_session,
        native_nv12_session,
        // Only the direct-SDK NVENC backend takes a packed 10-bit PQ CUDA payload.
        // Without it HDR capture stays on the CPU path.
        hdr_cuda_ok: pf_encode::linux_hdr_cuda_ok(),
        nvenc_raw_dmabuf: pf_encode::linux_nvenc_raw_dmabuf_ok(),
        gamescope_tiled: false,
        encoder_modifiers,
    }
}

/// Live capturer for a client-sized monitor via the xdg ScreenCast portal.
/// Pass `want_hdr` only when the session negotiated HDR and the mirrored monitor
/// is in HDR mode ([`pf_capture::gnome_hdr_monitor_active`]). Pass
/// `want_metadata_cursor` only when the encode backend composites
/// `CapturedFrame::cursor`; otherwise the portal embeds the pointer and no
/// backend × cursor-mode pair streams cursorless.
#[cfg(target_os = "linux")]
pub fn open_portal_monitor(
    want_hdr: bool,
    want_metadata_cursor: bool,
) -> Result<Box<dyn Capturer>> {
    // RemoteDesktop-capable desktops (KWin/GNOME) inherit that grant headlessly.
    // wlroots/Sway has no RemoteDesktop portal, so use a plain ScreenCast session.
    let anchored = crate::inject::default_backend() == crate::inject::Backend::Libei;
    // Monitor mirrors never carry the native PyroWave plane (GameStream protocol).
    // Native NV12 stays off: this path does not resolve the codec, and GNOME/KWin
    // do not produce NV12 anyway.
    pf_capture::open_portal_monitor(
        anchored,
        want_hdr,
        want_metadata_cursor,
        zero_copy_policy(false, false, None, 8, want_hdr),
    )
}

#[cfg(not(target_os = "linux"))]
pub fn open_portal_monitor(
    _want_hdr: bool,
    _want_metadata_cursor: bool,
) -> Result<Box<dyn Capturer>> {
    anyhow::bail!("portal capture requires Linux (xdg-desktop-portal + PipeWire)")
}

/// One [`capture_virtual_output`] request, shared by every platform
/// implementation so the facade takes two arguments. `codec` is the session's
/// resolved encoder — Linux uses it to seed the gamescope tiled offer, other
/// platforms ignore it. `kwin`/`gamescope` carry PipeWire producer contracts
/// node ids and remote fds cannot reveal. `stream_hz` is the wire rate, which a display
/// created at a multiple of it cannot tell the capture. `screen` is the session's screen the
/// output shows, `0` the first: absolute input for it aims at this head. `gamescope_cursor`
/// reads the pointer from the output's nested Xwaylands (Linux; ignored elsewhere).
pub(crate) struct VirtualCaptureRequest {
    pub output: OutputFormat,
    pub codec: Option<crate::encode::Codec>,
    pub capture: crate::session_plan::CaptureBackend,
    pub kwin: bool,
    pub gamescope: bool,
    pub stream_hz: u32,
    pub screen: u8,
    pub gamescope_cursor: bool,
}

/// A live output's metadata without its keepalive: what [`capture_virtual_output`] needs to
/// attach a second time, once the old capturer hands the keepalive back.
#[cfg(target_os = "linux")]
#[derive(Clone)]
pub(crate) struct OutputLease {
    node_id: u32,
    preferred_mode: Option<(u32, u32, u32)>,
    ownership: pf_vdisplay::DisplayOwnership,
    pool_gen: Option<u64>,
    output_name: Option<String>,
    input_output: Option<String>,
    seat: Option<String>,
    pid: Option<u32>,
}

#[cfg(target_os = "linux")]
impl OutputLease {
    /// `None` on the portal path: its remote fd cannot be re-derived from the metadata.
    pub(crate) fn of(vout: &crate::vdisplay::VirtualOutput) -> Option<OutputLease> {
        vout.remote_fd.is_none().then(|| OutputLease {
            node_id: vout.node_id,
            preferred_mode: vout.preferred_mode,
            ownership: vout.ownership,
            pool_gen: vout.pool_gen,
            output_name: vout.output_name.clone(),
            input_output: vout.input_output.clone(),
            seat: vout.seat.clone(),
            pid: vout.pid,
        })
    }

    /// The gamescope instance this output belongs to; `None` for the box's own session.
    pub(crate) fn seat(&self) -> Option<&str> {
        self.seat.as_deref()
    }

    /// The output as a fresh capture sees it. Never a birth-size gate: the output already
    /// sits at its mode.
    pub(crate) fn into_output(self, keepalive: Box<dyn Send>) -> crate::vdisplay::VirtualOutput {
        crate::vdisplay::VirtualOutput {
            node_id: self.node_id,
            remote_fd: None,
            preferred_mode: self.preferred_mode,
            keepalive,
            ownership: self.ownership,
            reused_gen: None,
            pool_gen: self.pool_gen,
            expect_exact_dims: false,
            output_name: self.output_name,
            input_output: self.input_output,
            seat: self.seat,
            pid: self.pid,
        }
    }
}

/// Capturer from an already-created [`crate::vdisplay::VirtualOutput`], which it owns. A
/// named output is offered to direct capture first; `pf_capture::open_virtual_output` picks.
/// Direct capture probes its consumer; PipeWire probes only level-21 gamescope and
/// non-gamescope PyroWave. Ordinary KWin, Mutter, and older gamescope keep their LINEAR offer.
#[cfg(target_os = "linux")]
pub fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    request: VirtualCaptureRequest,
) -> Result<Box<dyn Capturer>> {
    let VirtualCaptureRequest {
        output: want,
        codec,
        capture: _capture,
        kwin,
        gamescope,
        stream_hz,
        screen,
        gamescope_cursor,
    } = request;
    // `want.hdr` was resolved by [`capturer_supports_hdr_for`] before the Welcome.
    // Aim absolute input at THIS head: EXTEND backends sit beside the operator's
    // screens. `None` (Mutter/gamescope) CLEARS a stale name, e.g. after a Game-Mode
    // switch Hyprland → gamescope has removed `PF-…`.
    crate::inject::aim_at_head(
        screen,
        vout.output_name.clone().or(vout.input_output.clone()),
        vout.preferred_mode,
    );
    // The encoder modifier probe keys on bit depth: HDR and 10-bit SDR both ride
    // the packed 10-bit fourccs.
    let bit_depth = if want.hdr || want.ten_bit_sdr { 10 } else { 8 };
    let gamescope_tiled = gamescope && pf_vdisplay::gamescope_tiled_capture(None);
    // NVENC copies a producer's planar frame; an older gamescope composites it through system
    // RAM, which costs more than the conversion it saves. VAAPI keeps its own rule.
    let nv12_native = want.nv12_native
        && (!gamescope
            || crate::encode::linux_zero_copy_is_vaapi()
            || pf_vdisplay::gamescope_planar_capture(None));
    let modifier_codec = if gamescope {
        gamescope_tiled.then_some(codec).flatten()
    } else if want.pyrowave {
        codec
    } else {
        None
    };
    // Direct capture's modifier lists come from the session codec, PipeWire's from
    // `modifier_codec`.
    let (pyrowave, hdr) = (want.pyrowave, want.hdr);
    let direct = vout
        .output_name
        .clone()
        .map(|output_name| pf_capture::DirectOutput {
            output_name,
            policy: Box::new(move || {
                zero_copy_policy(pyrowave, nv12_native, codec, bit_depth, hdr)
            }),
        });
    let seat = vout.seat.clone();
    let gamescope_cursor = gamescope_cursor.then(|| {
        std::sync::Arc::new(move || pf_vdisplay::gamescope_xwayland_cursor_targets(seat.as_deref()))
            as pf_capture::GamescopeCursorTargets
    });
    let producer = if kwin {
        pf_capture::Producer::Kwin
    } else if gamescope {
        pf_capture::Producer::Gamescope
    } else {
        pf_capture::Producer::Other
    };
    pf_capture::open_virtual_output(
        vout.remote_fd,
        vout.node_id,
        vout.preferred_mode,
        vout.keepalive,
        pf_capture::VirtualOutputOpts {
            allow_zerocopy: want.gpu,
            want_444: want.chroma_444,
            want_hdr: want.hdr,
            ten_bit_sdr: want.ten_bit_sdr,
            sdr10_native: want.sdr10_native,
            expect_exact_dims: vout.expect_exact_dims,
            producer,
            stream_hz,
            gamescope_cursor,
            direct,
            policy: pf_capture::ZeroCopyPolicy {
                // No route here. A wrong "foreign" only keeps the LINEAR offer.
                gamescope_tiled,
                ..zero_copy_policy(pyrowave, nv12_native, modifier_codec, bit_depth, hdr)
            },
        },
    )
}

/// Can the native-plane source this session will drive deliver 10-bit PQ/BT.2020?
/// Capture half of the punktfunk/1 bit-depth gate (`native::handshake`).
///
/// Must be truthful before spawn: `bit_depth` is decided before the display
/// exists, and PQ frames to an 8-bit encoder are a hard error (`pf-encode`
/// Linux encoder). `pf_capture::capturer_supports_hdr()` cannot answer this on
/// Linux — it depends on the resolved compositor and the installed gamescope.
///
/// Windows: IDD-push enables advanced colour, so the platform answer.
/// Linux + gamescope: host knob, `packaging/gamescope` 10-bit BT.2020/PQ, a
/// spawned (not attached-foreign) sub-mode, and no earlier virtual-output HDR
/// downgrade latched. Hyprland and wlroots: direct capture of a head lit in
/// BT.2020 PQ ([`direct_capture_hdr`]). Anything else on Linux is 8-bit; GNOME
/// 50+ portal HDR is the GameStream plane (`gamestream::host_hdr_capable`).
pub fn capturer_supports_hdr_for(
    compositor: Option<crate::vdisplay::Compositor>,
    gamescope_route: Option<&crate::vdisplay::GamescopeRoute>,
) -> bool {
    #[cfg(target_os = "linux")]
    {
        use crate::vdisplay::Compositor;
        match compositor {
            Some(Compositor::Gamescope) => {
                return pf_host_config::config().gamescope_hdr
                    && pf_vdisplay::gamescope_hdr_available(gamescope_route)
                    && !pf_capture::hdr_capture_failed(pf_capture::HdrSource::VirtualOutput);
            }
            Some(c @ (Compositor::Hyprland | Compositor::Wlroots)) => {
                return direct_capture_hdr(c);
            }
            _ => {}
        }
    }
    let _ = (compositor, gamescope_route);
    pf_capture::capturer_supports_hdr()
}

/// Direct capture's HDR answer. A pinned head must already be lit in BT.2020 PQ (its
/// `wp_color_management_v1` description); a Hyprland headless output is created in HDR
/// by its backend. Needs an encoder that reads packed 10-bit RGB off a dmabuf, and no
/// virtual-output latch from a rule or offer that did not take.
///
/// The compositor must advertise the globals an HDR capture binds: the portal a failed
/// open falls back to is SDR, and the client already renders by the Welcome's colour.
#[cfg(target_os = "linux")]
fn direct_capture_hdr(compositor: crate::vdisplay::Compositor) -> bool {
    if !pf_capture::direct_capture()
        || pf_capture::hdr_capture_failed(pf_capture::HdrSource::VirtualOutput)
    {
        return false;
    }
    let ingests_rgb10 = crate::encode::linux_zero_copy_is_vaapi()
        || pf_encode::linux_hdr_cuda_ok()
        || pf_encode::linux_nvenc_raw_dmabuf_ok();
    if !ingests_rgb10 || !pf_capture::direct_hdr_globals() {
        return false;
    }
    match pf_vdisplay::capture_monitor() {
        Some(head) => pf_capture::output_hdr(&head) == Some(true),
        None => compositor == crate::vdisplay::Compositor::Hyprland,
    }
}

/// Does the source composite 10-bit SDR itself? Our gamescope from `+pfhdr26`, which offers
/// its 10-bit formats under BT.709, and Windows composing the virtual display in SDR wide
/// colour (FP16). Such a session needs no widening opt-in.
pub fn capturer_delivers_sdr10_for(
    compositor: Option<crate::vdisplay::Compositor>,
    gamescope_route: Option<&crate::vdisplay::GamescopeRoute>,
) -> bool {
    match compositor {
        Some(crate::vdisplay::Compositor::Gamescope) => {
            pf_vdisplay::gamescope_sdr10_capture(gamescope_route)
        }
        Some(crate::vdisplay::Compositor::Windows) => pf_capture::capturer_delivers_sdr10(),
        _ => false,
    }
}

#[cfg(target_os = "windows")]
pub fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    request: VirtualCaptureRequest,
) -> Result<Box<dyn Capturer>> {
    let VirtualCaptureRequest {
        output: want,
        // Linux-only (encoder-proved tiled offer); IDD-push negotiates no dmabuf modifiers.
        codec: _codec,
        capture: _capture,
        // Linux-only (`SPA_META_Cursor`, pool depth). IDD-push has no such meta; hide is
        // CURSOR_SUPPRESSED.
        kwin: _kwin,
        gamescope: _gamescope,
        // The driver paces its own encodes (`pf_frame::pace`).
        stream_hz,
        screen,
        // gamescope is Linux-only.
        gamescope_cursor: _gamescope_cursor,
    } = request;
    // A monitor this host did not create is captured by a worker; the driver has no part in it.
    if vout.ownership == crate::vdisplay::DisplayOwnership::External {
        return capture_external_output(vout, want, stream_hz, screen);
    }
    let target = vout.win_capture.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "pf-vdisplay target not yet an active display path (activation failed — see the \
             virtual-display warnings above)"
        )
    })?;
    // Aim absolute input at THIS display: a physical monitor can share the desktop (Extend, or
    // an Exclusive isolate degraded to the keep-physicals fallback).
    crate::inject::aim_at_target(
        screen,
        pf_win_display::win_display::CcdTargetKey::new(target.adapter_luid, target.target_id),
        vout.preferred_mode,
    );
    let pref = vout.preferred_mode;
    let keep = vout.keepalive;
    // Resolve the pf-vdisplay control device once and wrap its cursor IOCTLs for the
    // IDD-push capturer. This is the one host reach into `crate::vdisplay` the capturer
    // would otherwise make.
    let control = crate::vdisplay::manager::control_device_handle().ok_or_else(|| {
        anyhow::anyhow!(
            "pf-vdisplay control device not open (monitor not created via the manager?)"
        )
    })?;
    // Each closure clones the `Arc<ControlDevice>`, so the handle stays open for the closure's
    // life and closes when the manager retires it and the last session drops. An open control
    // handle vetoes the wake-from-sleep PnP cycle.

    // Presence of this closure opts the session into v5 cursor-channel delivery (capturer
    // creates CursorShm; driver declares the IddCx hardware cursor). An already-excluded target
    // gets one too — it is the shape source the pool's blend needs. Whether a CLIENT draws the
    // pointer is `want.hw_cursor`, passed separately: the channel alone does not say.
    let control_cursor = control.clone();
    let want_channel = want.hw_cursor || target.cursor_excluded;
    let cursor_sender: Option<pf_capture::CursorChannelSender> = want_channel.then(|| {
        std::sync::Arc::new(
            move |req: &pf_driver_proto::control::SetCursorChannelRequest| {
                crate::vdisplay::driver::send_cursor_channel(&control_cursor, req)
            },
        ) as pf_capture::CursorChannelSender
    });
    // The cursor render model (`IOCTL_SET_CURSOR_FORWARD`): `false` = the client draws no
    // pointer (capture model, UAC/Winlogon), so the driver blends the excluded one into its
    // frames. Built for every session: a channel-less reuse can still have a live cursor
    // worker from an earlier session. Never-declared targets answer NOT_FOUND, which the
    // capturer logs and ignores.
    let target_id = target.target_id;
    let control_forward = control.clone();
    let cursor_forward: Option<pf_capture::CursorForwardSender> = Some({
        std::sync::Arc::new(move |enable: bool| {
            let req = pf_driver_proto::control::SetCursorForwardRequest {
                target_id,
                enable: enable as u32,
            };
            crate::vdisplay::driver::send_cursor_forward(&control_forward, &req)
        }) as pf_capture::CursorForwardSender
    });
    // The driver's encoder opens and is driven over the same handle (`RemoteEncoder::open`).
    let control_open = control.clone();
    let set_encode: pf_capture::SetEncodeSender =
        std::sync::Arc::new(move |req: &pf_driver_proto::encode::SetEncodeRequest| {
            crate::vdisplay::driver::send_set_encode(&control_open, req)
        });
    let encode_ctl: pf_capture::EncodeCtlSender =
        std::sync::Arc::new(move |req: &pf_driver_proto::encode::EncodeCtlRequest| {
            crate::vdisplay::driver::send_encode_ctl(&control, req)
        });
    let opts = pf_capture::IddPushOpts {
        want_hdr: want.hdr,
        ten_bit_sdr: want.ten_bit_sdr,
        want_444: want.chroma_444,
        pyrowave: want.pyrowave,
        cursor_sender,
        cursor_forward,
        forwards_to_client: want.hw_cursor,
        set_encode,
        encode_ctl,
    };
    pf_capture::open_idd_push(target, pref, keep, opts)
        .map_err(|(e, _keep)| e.context("IDD-push capture open (no fallback)"))
}

/// Capture a monitor this session does not own: start a capture worker as the signed-in user
/// and open the monitor in it. The absolute-input aim moves to that monitor, as the IDD-push
/// open does for its own.
///
/// A mirror's worker gets the GPU scheduling class a session's encoder gets. A viewer of
/// another session's display keeps the default, below the driver that encodes for its owner.
#[cfg(target_os = "windows")]
fn capture_external_output(
    vout: crate::vdisplay::VirtualOutput,
    want: OutputFormat,
    stream_hz: u32,
    screen: u8,
) -> Result<Box<dyn Capturer>> {
    use anyhow::Context as _;
    let target = vout
        .win_capture
        .clone()
        .context("the mirrored monitor carries no capture target")?;
    let key = pf_win_display::win_display::CcdTargetKey::new(target.adapter_luid, target.target_id);
    let hdr = pf_win_display::display_events::snapshot_or_query()
        .target(key)
        .and_then(|t| t.hdr)
        .unwrap_or(false);
    crate::inject::aim_at_target(screen, key, vout.preferred_mode);
    let worker = crate::windows::capture_worker::spawn().context("start the capture worker")?;
    let source = pf_capture::WgcSource {
        gdi_name: target.gdi_name,
        target_id: target.target_id,
        hdr,
    };
    let own_priority = crate::session_plan::mirrored();
    pf_capture::open_wgc(
        worker,
        source,
        want.hdr,
        stream_hz,
        own_priority,
        vout.keepalive,
    )
    .context("WGC capture open (no fallback)")
}

/// Open a remote-encoder source's encoder (the driver's, or a capture worker's) at the depth the
/// display composes now, and record it under the name of what opened. The heap is sized from
/// the opening rate; ABR climbs past twice it eat the burst margin. `client_hdr` replaces the
/// capturer's HDR baseline, as the stream loop does, so the first IDR carries the client's panel.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
pub fn open_driver_encoder(
    plan: &crate::session_plan::SessionPlan,
    capturer: &dyn Capturer,
    size: (u32, u32),
    fps: u32,
    bitrate_bps: u64,
    bit_depth: u8,
    client_hdr: Option<pf_frame::HdrMeta>,
    wire_seq_base: u32,
) -> Result<Box<dyn crate::encode::Encoder>> {
    use crate::encode::WindowsBackend;
    use pf_driver_proto::encode::backend as be;
    let backend = match crate::encode::windows_resolved_backend() {
        WindowsBackend::Nvenc => Some(be::NVENC),
        WindowsBackend::Amf => Some(be::AMF),
        WindowsBackend::Qsv => Some(be::QSV),
        WindowsBackend::MediaFoundation => Some(be::MEDIA_FOUNDATION),
        WindowsBackend::Software => None,
    };
    // The capturer reports HDR metadata exactly while the display composes FP16.
    let colour = capturer.colour();
    let (hdr, bit_depth) =
        crate::session_plan::live_depth(plan.hdr, colour.hdr_meta.is_some(), bit_depth);
    let params = pf_capture::DriverEncodeParams::for_session(pf_capture::SessionFacts {
        codec: plan.codec,
        chroma: plan.chroma,
        backend,
        hdr,
        bit_depth,
        colour,
        client_hdr,
        size,
        fps,
        bitrate_bps,
        slices: plan.slices,
        wire_chunk: plan.wire_chunk,
        wire_seq_base,
    })?;
    let remote = capturer.remote_encoder().ok_or_else(|| {
        anyhow::anyhow!("driver encode: the capture source runs no remote encoder")
    })?;
    let enc = remote.open(&params)?;
    let label = remote.site().label(enc.telemetry().map(|t| t.backend));
    Ok(crate::encode::track_session(enc, label))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capture_virtual_output(
    _vout: crate::vdisplay::VirtualOutput,
    request: VirtualCaptureRequest,
) -> Result<Box<dyn Capturer>> {
    let VirtualCaptureRequest {
        output: _want,
        codec: _codec,
        capture: _capture,
        kwin: _kwin,
        gamescope: _gamescope,
        stream_hz: _stream_hz,
        screen: _screen,
        gamescope_cursor: _gamescope_cursor,
    } = request;
    anyhow::bail!("virtual-output capture requires Linux or Windows")
}

#[cfg(all(test, target_os = "windows"))]
mod live_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Live IDD-push ring: a session-shaped capturer must deliver Source frames.
    /// Elevated console session; host service stopped.
    #[test]
    #[ignore = "live: needs the pf-vdisplay driver, a console session, the host service stopped"]
    fn live_idd_push_ring_delivers_source_frames() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("info,pf_capture=debug"))
            .with_test_writer()
            .try_init();
        let mut vd = crate::vdisplay::open(crate::vdisplay::Compositor::Windows)
            .expect("open the pf-vdisplay backend");
        let vout = vd
            .create(punktfunk_core::Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create a virtual display");
        let want = OutputFormat {
            gpu: true,
            hdr: false,
            ten_bit_sdr: false,
            sdr10_native: false,
            chroma_444: false,
            pyrowave: false,
            nv12_native: false,
            hw_cursor: false,
        };
        let mut cap = capture_virtual_output(
            vout,
            VirtualCaptureRequest {
                output: want,
                codec: None,
                capture: crate::session_plan::CaptureBackend::IddPush,
                kwin: false,
                gamescope: false,
                stream_hz: 60,
                screen: 0,
                gamescope_cursor: false,
            },
        )
        .expect("open the IDD-push capturer");
        // A static desktop composes nothing: walk the pointer so every tick has a new image.
        let (mut source, mut regen, mut repeat, mut errors) = (0u32, 0u32, 0u32, 0u32);
        // `PF_LIVE_RING_SECS` stretches the run (default 12 s) so slower behaviours — the
        // exclusive watchdog's breaker on a relight fight — have time to show in the log.
        let secs: u64 = std::env::var("PF_LIVE_RING_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(12);
        let start = Instant::now();
        let mut x = 100i32;
        while start.elapsed() < Duration::from_secs(secs) {
            x = 100 + ((x - 100 + 7) % 600);
            // SAFETY: plain FFI; the pointer is parked on the operator's desktop for the test.
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SetCursorPos(x, 100 + x / 3);
            }
            if let Some(feed) = cap.feed() {
                feed.wait_arrival(Instant::now() + Duration::from_millis(40));
            }
            match cap.try_latest() {
                Ok(Some(f)) => match f.provenance.origin {
                    pf_frame::FrameOrigin::Source => source += 1,
                    _ => regen += 1,
                },
                Ok(None) => {
                    repeat += 1;
                    std::thread::sleep(Duration::from_millis(8));
                }
                Err(e) => {
                    errors += 1;
                    assert!(errors <= 3, "the ring keeps failing: {e:#}");
                }
            }
        }
        assert!(
            source >= 60,
            "expected a steady stream of NEW source frames from the ring, got {source} \
             (regen={regen} repeat={repeat} errors={errors} in {:?})",
            start.elapsed()
        );
        drop(cap);
        drop(vd);
    }

    /// Freeze `pid` for `hold` by debugger attach (every thread stops at the attach event and
    /// stays stopped until the SAME thread detaches), on a helper thread. `attached` flips once
    /// the freeze took; kill-on-exit is off so a test panic never takes the debuggee down.
    fn freeze_for(
        pid: u32,
        hold: Duration,
        attached: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        use windows::Win32::System::Diagnostics::Debug::{
            DebugActiveProcess, DebugActiveProcessStop, DebugSetProcessKillOnExit,
        };
        std::thread::spawn(move || {
            // SAFETY: plain debug-API FFI on a pid the caller owns for the test's life.
            unsafe {
                let _ = DebugSetProcessKillOnExit(false);
                if DebugActiveProcess(pid).is_err() {
                    return;
                }
                attached.store(true, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(hold);
                let _ = DebugActiveProcessStop(pid);
            }
        })
    }

    /// Live WUDFHost freeze: the same capturer must recover or end with a typed fault.
    /// Default hold 18 s (`PF_LIVE_FREEZE_SECS`). Elevated console; host service stopped.
    #[test]
    #[ignore = "live: freezes the pf-vdisplay WUDFHost for ~18 s; elevated console session, host service stopped"]
    fn live_frozen_driver_host_ends_the_plane_with_a_typed_fault() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("info,pf_capture=debug"))
            .with_test_writer()
            .try_init();
        let hold = Duration::from_secs(
            std::env::var("PF_LIVE_FREEZE_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(18),
        );
        let mut vd = crate::vdisplay::open(crate::vdisplay::Compositor::Windows)
            .expect("open the pf-vdisplay backend");
        let vout = vd
            .create(punktfunk_core::Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create a virtual display");
        let pid = vout
            .win_capture
            .as_ref()
            .expect("the virtual display carries its capture target")
            .wudf_pid;
        assert_ne!(pid, 0, "the driver reported no WUDFHost pid");
        let want = OutputFormat {
            gpu: true,
            hdr: false,
            ten_bit_sdr: false,
            sdr10_native: false,
            chroma_444: false,
            pyrowave: false,
            nv12_native: false,
            hw_cursor: false,
        };
        let mut cap = capture_virtual_output(
            vout,
            VirtualCaptureRequest {
                output: want,
                codec: None,
                capture: crate::session_plan::CaptureBackend::IddPush,
                kwin: false,
                gamescope: false,
                stream_hz: 60,
                screen: 0,
                gamescope_cursor: false,
            },
        )
        .expect("open the IDD-push capturer");
        let mut x = 100i32;
        let mut step = |cap: &mut Box<dyn Capturer>| -> Result<bool> {
            x = 100 + ((x - 100 + 7) % 600);
            // SAFETY: plain FFI; the pointer is parked on the operator's desktop for the test.
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SetCursorPos(x, 100 + x / 3);
            }
            if let Some(feed) = cap.feed() {
                feed.wait_arrival(Instant::now() + Duration::from_millis(40));
            }
            match cap.try_latest()? {
                Some(f) => Ok(f.provenance.origin == pf_frame::FrameOrigin::Source),
                None => {
                    std::thread::sleep(Duration::from_millis(8));
                    Ok(false)
                }
            }
        };
        // Warm: the ring must be delivering before anything is broken.
        let warm_until = Instant::now() + Duration::from_secs(3);
        let mut warm = 0u32;
        while Instant::now() < warm_until {
            if step(&mut cap).expect("capture before the fault") {
                warm += 1;
            }
        }
        assert!(warm >= 10, "no steady source before the fault (got {warm})");
        // Attaching to WUDFHost (LocalService) needs it even from an elevated console.
        pf_frame::privilege::enable("SeDebugPrivilege").expect("enable SeDebugPrivilege");
        let attached = Arc::new(AtomicBool::new(false));
        let freezer = freeze_for(pid, hold, attached.clone());
        let t_freeze = Instant::now();
        while !attached.load(Ordering::SeqCst) && t_freeze.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            attached.load(Ordering::SeqCst),
            "debugger attach to WUDFHost pid {pid} did not take"
        );
        let (mut during, mut after, mut outage, mut error) = (0u32, 0u32, None, None);
        let mut first_after: Option<Duration> = None;
        while t_freeze.elapsed() < hold + Duration::from_secs(60) {
            match step(&mut cap) {
                Ok(true) if t_freeze.elapsed() < hold => during += 1,
                Ok(true) => {
                    after += 1;
                    first_after.get_or_insert(t_freeze.elapsed());
                }
                Ok(false) => {}
                Err(e) => {
                    error = Some(format!("{e:#}"));
                    break;
                }
            }
            let recovered = cap.events().into_iter().find_map(|e| match e {
                pf_capture::SourceEvent::RecoveredOutage(o) => Some(o),
                _ => None,
            });
            if let Some(o) = recovered {
                outage = Some(o);
                break;
            }
        }
        let ended = t_freeze.elapsed();
        let _ = freezer.join();
        // An in-flight slot or two can still drain after the attach; more means the worker ran.
        assert!(
            during <= 2,
            "a frozen worker cannot keep publishing: {during} source frames during the freeze \
             (after={after} first_after={first_after:?} outage={outage:?} error={error:?} \
             ended_after={ended:?})"
        );
        match (outage, &error) {
            (Some(outage), _) => {
                assert!(
                    outage >= Duration::from_secs(15),
                    "a recovered outage must span the stall floor, got {outage:?}"
                );
                assert!(after >= 3, "real source frames must resume after the thaw");
            }
            (None, Some(e)) => {
                // Death watch ("WUDFHost … exited") or `SourceStalled` ("no source frame for Ns").
                assert!(
                    e.contains("WUDFHost") || e.contains("no source frame"),
                    "the plane must end with a typed driver/source fault, got: {e}"
                );
            }
            (None, None) => panic!(
                "neither a recovery nor a typed end within {:?} — the stale plane is exactly \
                 what must never happen",
                hold + Duration::from_secs(60)
            ),
        }
        drop(cap);
        drop(vd);
    }
}
