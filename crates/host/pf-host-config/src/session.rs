//! The live graphical session as the host last detected it.
//!
//! pf-vdisplay's `apply_session_env` stores it; the Wayland backends and every desktop-facing
//! child read `WAYLAND_DISPLAY` from here, never from the process environment. The other three
//! session variables are still written to the environment.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Env that points a backend at the detected session. `hyprland_signature` / `sway_socket` are
/// per-spawn (`Command::env`): `hyprctl` / `swaymsg` are children we launch, not process-wide
/// readers.
#[derive(Clone, Debug, Default)]
pub struct SessionEnv {
    /// `WAYLAND_DISPLAY` of the live compositor. Detection leaves it `None` for Gaming-attach
    /// and Mutter, which never talk Wayland to us; [`set`] then keeps the last one.
    pub wayland_display: Option<String>,
    /// Per-user runtime dir (PipeWire + session bus). Never a world-writable `/tmp`.
    pub xdg_runtime_dir: String,
    pub dbus_session_bus_address: String,
    /// `XDG_CURRENT_DESKTOP` (KDE/GNOME/sway/Hyprland/gamescope). Drives portal/EIS routing;
    /// xdph keys Hyprland-specific behavior off `Hyprland`.
    pub xdg_current_desktop: Option<String>,
    /// `HYPRLAND_INSTANCE_SIGNATURE` (`Some` only on Hyprland). Handed to `hyprctl` per spawn
    /// so a systemd `--user` host need not inherit the session env.
    pub hyprland_signature: Option<String>,
    /// `SWAYSOCK` (`Some` only on sway). Same per-spawn handoff. `None` on river.
    pub sway_socket: Option<String>,
}

impl SessionEnv {
    /// Where the compositor listens: the name under the runtime dir, or the name itself when it
    /// is absolute.
    pub fn wayland_socket(&self) -> Option<PathBuf> {
        let name = self.wayland_display.as_deref()?;
        Some(Path::new(&self.xdg_runtime_dir).join(name))
    }
}

static LIVE: RwLock<Option<Arc<SessionEnv>>> = RwLock::new(None);

/// The stored session. `None` until the first [`set`].
pub fn get() -> Option<Arc<SessionEnv>> {
    LIVE.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Store the detected session. `live` is false when detection found no compositor.
pub fn set(env: SessionEnv, live: bool) {
    let mut slot = LIVE.write().unwrap_or_else(|e| e.into_inner());
    let last = match slot.as_deref() {
        Some(prev) => prev.wayland_display.clone(),
        None => inherited(),
    };
    *slot = Some(Arc::new(carry(env, live, last)));
}

/// A live session that names no socket keeps the last one; no live session clears it.
fn carry(mut env: SessionEnv, live: bool, last: Option<String>) -> SessionEnv {
    if !live {
        env.wayland_display = None;
    } else if env.wayland_display.is_none() {
        env.wayland_display = last;
    }
    env
}

fn inherited() -> Option<String> {
    std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|s| !s.is_empty())
}

/// `WAYLAND_DISPLAY` as the host sees it: the stored session's, or the one this process
/// started with while nothing is stored.
pub fn wayland_display() -> Option<String> {
    match get() {
        Some(env) => env.wayland_display.clone(),
        None => inherited(),
    }
}

/// The live compositor's socket, connected. `None` while nothing is stored: the caller then
/// connects from the environment. A stored session with no socket is `NotFound`.
#[cfg(unix)]
pub fn wayland_stream() -> Option<std::io::Result<std::os::unix::net::UnixStream>> {
    let env = get()?;
    Some(match env.wayland_socket() {
        Some(path) => std::os::unix::net::UnixStream::connect(path),
        None => Err(std::io::ErrorKind::NotFound.into()),
    })
}

/// Hand `cmd` the stored session's `WAYLAND_DISPLAY`, set or unset. A child inherits the other
/// session variables from the process environment.
pub fn export_to(cmd: &mut std::process::Command) -> &mut std::process::Command {
    if let Some(env) = get() {
        match &env.wayland_display {
            Some(name) => cmd.env("WAYLAND_DISPLAY", name),
            None => cmd.env_remove("WAYLAND_DISPLAY"),
        };
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: Option<&str>) -> SessionEnv {
        SessionEnv {
            wayland_display: name.map(str::to_string),
            xdg_runtime_dir: "/run/user/1000".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_socketless_live_session_keeps_the_last_socket() {
        let last = Some("wayland-0".to_string());
        let kept = carry(named(None), true, last.clone());
        assert_eq!(kept.wayland_display.as_deref(), Some("wayland-0"));
        let replaced = carry(named(Some("wayland-1")), true, last.clone());
        assert_eq!(replaced.wayland_display.as_deref(), Some("wayland-1"));
        assert_eq!(carry(named(None), false, last).wayland_display, None);
    }

    #[test]
    fn the_socket_is_the_name_under_the_runtime_dir_unless_absolute() {
        let socket = |name| named(name).wayland_socket();
        assert_eq!(
            socket(Some("wayland-0")),
            Some("/run/user/1000/wayland-0".into())
        );
        assert_eq!(
            socket(Some("/tmp/kwin.sock")),
            Some("/tmp/kwin.sock".into())
        );
        assert_eq!(socket(None), None);
    }
}
