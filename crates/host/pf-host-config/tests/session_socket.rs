//! The stored session against a real socket. Its own process: the store is process-global.
#![cfg(unix)]

use pf_host_config::session::{self, SessionEnv};
use std::ffi::OsStr;
use std::os::unix::net::UnixListener;
use std::process::Command;

fn wayland_of(cmd: &Command) -> Option<Option<&OsStr>> {
    cmd.get_envs()
        .find(|(name, _)| *name == "WAYLAND_DISPLAY")
        .map(|(_, value)| value)
}

#[test]
fn a_stored_session_decides_the_socket_and_the_childs_variable() {
    let dir = std::env::temp_dir().join(format!("pf-session-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let listener = UnixListener::bind(dir.join("wayland-test")).unwrap();
    let stored = |name: Option<&str>| SessionEnv {
        wayland_display: name.map(str::to_string),
        xdg_runtime_dir: dir.to_str().unwrap().to_string(),
        ..Default::default()
    };

    // Nothing stored: the caller connects from the environment, a child inherits.
    assert!(session::wayland_stream().is_none());
    assert_eq!(
        wayland_of(session::export_to(&mut Command::new("true"))),
        None
    );

    session::set(stored(Some("wayland-test")), true);
    session::wayland_stream().unwrap().unwrap();
    listener.accept().unwrap();
    let mut child = Command::new("true");
    assert_eq!(
        wayland_of(session::export_to(&mut child)),
        Some(Some(OsStr::new("wayland-test")))
    );

    // A live session that names no socket keeps the last one.
    session::set(stored(None), true);
    assert_eq!(session::wayland_display().as_deref(), Some("wayland-test"));

    // No live session: no socket, and a child's inherited variable is removed.
    session::set(stored(None), false);
    assert!(session::wayland_stream().unwrap().is_err());
    assert_eq!(
        wayland_of(session::export_to(&mut Command::new("true"))),
        Some(None)
    );

    std::fs::remove_dir_all(&dir).unwrap();
}
