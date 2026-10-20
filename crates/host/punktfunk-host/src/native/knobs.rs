//! The `PUNKTFUNK_*` knobs the native plane reads once per process: [`HostKnobs`], handed to
//! each session. Only a value a restart alone can change lives here.

use super::Punktfunk1Source;
use std::time::Duration;

/// The knobs the process reads once.
pub(crate) struct HostKnobs {
    /// `PUNKTFUNK_SOURCE=software`: a software-encoded test picture instead of a display, so a
    /// headless box with the management API can prove a client end to end. Anything else is the
    /// display.
    pub(crate) source: Punktfunk1Source,
    /// `PUNKTFUNK_IDLE_TIMEOUT_MS`; `None` (unset/invalid/zero) = core default (8 s). Clamped
    /// downstream to ≥1 s with a keep-alive that scales, so a live session never false-closes.
    pub(crate) idle_timeout: Option<Duration>,
    /// `PUNKTFUNK_ACKED_REFS`: `on` holds confirmed references all session, `off` never takes
    /// them, `None` follows the link.
    pub(crate) acked_refs: Option<bool>,
    /// `PUNKTFUNK_IDD_ADAPTIVE`, on by default. Off skips the cadence adaptation: no cadence
    /// verdict for the ABR climb gate, and the encoder never escalates to pipelined retrieve.
    pub(crate) idd_adaptive: bool,
    /// `PUNKTFUNK_XBOX_BACKEND` as set (`hid` / `xusb`), for `gamepad::windows_xbox_hid`.
    #[cfg(target_os = "windows")]
    pub(crate) xbox_backend: Option<String>,
}

impl HostKnobs {
    /// The process's knobs, read on first use.
    pub(crate) fn get() -> &'static HostKnobs {
        static KNOBS: std::sync::OnceLock<HostKnobs> = std::sync::OnceLock::new();
        KNOBS.get_or_init(|| HostKnobs {
            source: match std::env::var("PUNKTFUNK_SOURCE").as_deref() {
                Ok("software") => Punktfunk1Source::Software,
                _ => Punktfunk1Source::Virtual,
            },
            idle_timeout: pf_host_config::knob("PUNKTFUNK_IDLE_TIMEOUT_MS")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .filter(|&ms| ms > 0)
                .map(Duration::from_millis),
            acked_refs: match std::env::var("PUNKTFUNK_ACKED_REFS").as_deref() {
                Ok("on") => Some(true),
                Ok("off") => Some(false),
                _ => None,
            },
            idd_adaptive: pf_host_config::env_on("PUNKTFUNK_IDD_ADAPTIVE").unwrap_or(true),
            #[cfg(target_os = "windows")]
            xbox_backend: std::env::var("PUNKTFUNK_XBOX_BACKEND").ok(),
        })
    }
}
