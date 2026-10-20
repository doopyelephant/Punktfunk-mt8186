//! Backend resolution: which encoder a session opens and what it can encode, asked by the host
//! before and during every session. `linux.rs` and `windows.rs` answer per OS; any other target
//! answers from the stub arm below.

use crate::*;
use anyhow::Result;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, OnceLock};

// One module per OS, each answering the same `pub(crate)` questions; its OS-only public API
// re-exports from here.
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(target_os = "windows")]
use windows as imp;

pub(crate) use imp::open;
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub use imp::*;

/// No GPU encode backend on this target: nothing is probed and nothing opens.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod imp {
    use super::*;

    pub(crate) fn open(_: &OpenParams) -> Result<(Box<dyn Encoder>, &'static str)> {
        anyhow::bail!("video encode requires Linux or Windows")
    }

    /// The unprobed advertisement: H.264 for a software pin, else HEVC.
    pub(crate) fn wire_caps() -> u8 {
        if matches!(
            pf_host_config::config().encoder_pref.as_str(),
            "software" | "sw" | "openh264"
        ) {
            punktfunk_core::quic::CODEC_H264
        } else {
            punktfunk_core::quic::CODEC_HEVC
        }
    }

    pub(crate) fn hevc_444() -> bool {
        false
    }

    pub(crate) fn ten_bit(_codec: Codec) -> bool {
        false
    }

    pub(crate) fn sdr10(_codec: Codec) -> bool {
        false
    }

    /// No resolver: only a software pin is a CPU path.
    pub(crate) fn is_gpu() -> bool {
        !matches!(
            pf_host_config::config().encoder_pref.as_str(),
            "software" | "sw" | "openh264"
        )
    }

    pub(crate) fn ingests_rgb_444() -> bool {
        false
    }
}

/// A probe result per key. The key carries the selected GPU, so a console preference change
/// re-probes.
type ProbeCache<K, V> = OnceLock<Mutex<HashMap<K, V>>>;

/// The console's Encoder row, part of every capability key: the row applies to the next
/// session, so a verdict probed under one backend must not answer for another.
fn encoder_pref() -> String {
    pf_host_config::config().encoder_pref.clone()
}

/// The console's Vulkan encoding row (`PUNKTFUNK_VULKAN_ENCODE`, default on): Vulkan Video
/// HEVC/AV1 on AMD/Intel, falling back to VAAPI on a failed open. Off is the VAAPI hatch. It
/// applies at the next session, so it keys every verdict that depends on it.
fn vulkan_row() -> bool {
    pf_host_config::row_bool("PUNKTFUNK_VULKAN_ENCODE")
}

/// `probe()` once per `key`, outside the lock. Concurrent first calls may both probe; the last
/// insert wins.
fn probe_cached<K: Eq + Hash, V: Copy>(
    cache: &ProbeCache<K, V>,
    key: K,
    probe: impl FnOnce() -> V,
) -> V {
    let cache = cache.get_or_init(Default::default);
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }
    let v = probe();
    cache.lock().unwrap().insert(key, v);
    v
}

/// `quic::CODEC_*` bits this host can emit on the native path, given the
/// resolved backend. Fed to [`punktfunk_core::quic::resolve_codec`].
///
/// Software is H.264 only. Probed backends advertise what the GPU encodes
/// ([`vaapi_codec_support`] / [`windows_codec_support`]); NVENC falls back to
/// the GameStream superset when the probe cannot answer. An empty probe
/// means the GPU was unusable at probe time, not that it encodes nothing —
/// fall back to the superset so auto clients still land on HEVC.
pub fn host_wire_caps() -> u8 {
    imp::wire_caps()
}

/// `quic::CODEC_*` bits of a [`CodecSupport`] probe, or `None` when it found
/// nothing — GPU unusable at probe time, not "zero codecs". Caller falls back
/// to the static superset.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn codec_support_wire_mask(caps: CodecSupport) -> Option<u8> {
    let mut m = 0u8;
    if caps.h264 {
        m |= punktfunk_core::quic::CODEC_H264;
    }
    if caps.h265 {
        m |= punktfunk_core::quic::CODEC_HEVC;
    }
    if caps.av1 {
        m |= punktfunk_core::quic::CODEC_AV1;
    }
    (m != 0).then_some(m)
}

/// Whether the active backend can emit 4:4:4 HEVC. Cached per selected GPU
/// before Welcome. 4:4:4 is HEVC-only; VAAPI/AMF/QSV must be probed, never
/// assumed. Non-HEVC is always `false`.
pub fn can_encode_444(codec: Codec) -> bool {
    match codec {
        // Own RGB→YCbCr CSC from a full-chroma source — no GPU encode probe.
        // See `design/pyrowave-444-hdr.md`.
        Codec::PyroWave => cfg!(any(target_os = "linux", target_os = "windows")),
        Codec::H265 if cfg!(any(target_os = "linux", target_os = "windows")) => {
            static CACHE: ProbeCache<(String, String), bool> = OnceLock::new();
            probe_cached(&CACHE, (pf_gpu::selection_key(), encoder_pref()), || {
                let supported = imp::hevc_444();
                tracing::info!(supported, "HEVC 4:4:4 encode capability probed");
                supported
            })
        }
        _ => false,
    }
}

/// Whether the active backend can emit 10-bit for `codec` (HEVC Main10 / AV1).
/// Cached per (GPU, codec, Vulkan encoding row) before Welcome, like [`can_encode_444`]. Without
/// this gate `PUNKTFUNK_10BIT` would negotiate 10-bit and then emit 8-bit
/// (label HDR / stream SDR).
pub fn can_encode_10bit(codec: Codec) -> bool {
    if !codec.supports_10bit() || !cfg!(any(target_os = "linux", target_os = "windows")) {
        return false;
    }
    if codec == Codec::PyroWave {
        // Wavelet is depth-agnostic; the CSC runs on the encoder's own Vulkan device, so
        // 10-bit needs no encode-profile probe — just the `pyrowave` backend existing on
        // this OS. See `design/pyrowave-444-hdr.md`.
        return cfg!(target_os = "windows") || cfg!(feature = "pyrowave");
    }
    // The Vulkan encoding row decides what Linux AMD/Intel opens, per session.
    static CACHE: ProbeCache<(String, &'static str, bool, String), bool> = OnceLock::new();
    probe_cached(
        &CACHE,
        (
            pf_gpu::selection_key(),
            codec.label(),
            vulkan_row(),
            encoder_pref(),
        ),
        || {
            let supported = imp::ten_bit(codec);
            tracing::info!(codec = ?codec, supported, "10-bit encode capability probed");
            supported
        },
    )
}

/// GPU-resident frames (software is the only CPU path). Single source for
/// [`pf_frame::OutputFormat`]'s `gpu` bit — capture must not re-derive it.
pub fn resolved_backend_is_gpu() -> bool {
    imp::is_gpu()
}

/// Encoder half of the 4:4:4 capture gate: ingest RGB and CSC to 4:4:4.
/// Only Windows NVENC. Linux 4:4:4 is capture-side (portal RGB → `yuv444p`).
pub fn resolved_backend_ingests_rgb_444() -> bool {
    imp::ingests_rgb_444()
}

/// Encoder half of the 10-bit SDR gate: this backend writes a 10-bit stream from the 8-bit
/// surface an SDR desktop captures.
///
/// Windows NVENC widens the driver's BGRA itself; Windows AMF and QSV take a BT.709 P010 the
/// driver produces, for HEVC and AV1. Linux direct-NVENC takes the plain 8-bit surface
/// and asks NVENC for 10-bit output. Linux VAAPI carries HEVC and Vulkan Video carries AV1, both
/// with the RGB→YUV matrix following the BT.709 colour, not depth. The GPU still has to pass
/// `can_encode_10bit`.
pub fn backend_carries_sdr10(codec: Codec) -> bool {
    imp::sdr10(codec)
}

// Ungated: the pure reconciliation table is tested on every CI leg, not just Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsBackend {
    Nvenc,
    Amf,
    Qsv,
    /// Media Foundation: any vendor's hardware MFT. Second rung under the native SDKs on
    /// x64, and the only hardware encoder on an Adreno adapter.
    MediaFoundation,
    Software,
}

/// PCI vendor a Windows hardware backend can open on (`None` for software).
/// Capture, virtual display, and encoder share one adapter.
pub fn windows_backend_vendor_id(backend: WindowsBackend) -> Option<u32> {
    match backend {
        WindowsBackend::Nvenc => Some(pf_gpu::VENDOR_NVIDIA),
        WindowsBackend::Amf => Some(pf_gpu::VENDOR_AMD),
        WindowsBackend::Qsv => Some(pf_gpu::VENDOR_INTEL),
        // Vendor-agnostic: every x64 vendor and Adreno ship an MFT, so an `mf` pin is
        // never contradicted by the selected adapter.
        WindowsBackend::MediaFoundation | WindowsBackend::Software => None,
    }
}

/// Pure half of [`windows_resolved_backend`]: reconcile an explicit
/// `PUNKTFUNK_ENCODER` pin with the selected adapter. A hardware pin whose
/// vendor contradicts the selected GPU is overridden (`derived` is lazy) —
/// honoring it can only fail. Software has no vendor and is always honored;
/// with no selected GPU a pin is trusted as-is. [`open_video`] warns on override.
pub fn resolve_windows_backend(
    pinned: Option<WindowsBackend>,
    selected_vendor_id: Option<u32>,
    derived: impl FnOnce() -> WindowsBackend,
) -> WindowsBackend {
    match (pinned, selected_vendor_id) {
        (None, _) => derived(),
        (Some(pin), Some(vendor)) => match windows_backend_vendor_id(pin) {
            Some(required) if required != vendor => derived(),
            _ => pin,
        },
        (Some(pin), None) => pin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin whose vendor contradicts the selected GPU is overridden — never
    /// "pin + proceed" (that feeds the reset ladder a deterministic failure).
    #[test]
    fn encoder_pin_reconciles_against_the_selected_adapter() {
        use WindowsBackend::*;
        let derived = |b: WindowsBackend| move || b;
        let unreachable = || -> WindowsBackend { panic!("derived must not be consulted") };
        assert_eq!(
            resolve_windows_backend(Some(Qsv), Some(pf_gpu::VENDOR_NVIDIA), derived(Nvenc)),
            Nvenc
        );
        assert_eq!(
            resolve_windows_backend(Some(Qsv), Some(pf_gpu::VENDOR_INTEL), unreachable),
            Qsv
        );
        assert_eq!(
            resolve_windows_backend(Some(Nvenc), None, unreachable),
            Nvenc
        );
        assert_eq!(
            resolve_windows_backend(Some(Software), Some(pf_gpu::VENDOR_NVIDIA), unreachable),
            Software
        );
        assert_eq!(
            resolve_windows_backend(None, Some(pf_gpu::VENDOR_AMD), derived(Amf)),
            Amf
        );
        assert_eq!(
            resolve_windows_backend(None, None, derived(Software)),
            Software
        );
        // MF has no vendor, so no adapter can contradict the pin — that is the whole
        // point of the rung: it opens on NVIDIA, AMD, Intel, and Adreno alike.
        for vendor in [
            pf_gpu::VENDOR_NVIDIA,
            pf_gpu::VENDOR_AMD,
            pf_gpu::VENDOR_INTEL,
            pf_gpu::VENDOR_QUALCOMM,
        ] {
            assert_eq!(
                resolve_windows_backend(Some(MediaFoundation), Some(vendor), unreachable),
                MediaFoundation
            );
        }
        assert_eq!(
            resolve_windows_backend(
                None,
                Some(pf_gpu::VENDOR_QUALCOMM),
                derived(MediaFoundation)
            ),
            MediaFoundation
        );
        assert_eq!(windows_backend_vendor_id(MediaFoundation), None);
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn codec_support_wire_mask_maps_the_probe_to_bits() {
        use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC};
        let all = CodecSupport {
            h264: true,
            h265: true,
            av1: true,
        };
        assert_eq!(
            codec_support_wire_mask(all),
            Some(CODEC_H264 | CODEC_HEVC | CODEC_AV1)
        );
        let hevc_only = CodecSupport {
            h264: false,
            h265: true,
            av1: false,
        };
        assert_eq!(codec_support_wire_mask(hevc_only), Some(CODEC_HEVC));
        // All-false = GPU unusable, not "zero codecs" — `None` → static superset.
        let none = CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        };
        assert_eq!(codec_support_wire_mask(none), None);
    }
}
