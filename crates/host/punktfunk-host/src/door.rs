//! **Reachable without logging in** on Linux: the box's host moves from the owner's session to a
//! system service, the door (`serve --door`), and back.
//!
//! This process is unprivileged either way. A root oneshot does the move
//! (`packaging/linux/door-helper`), and polkit lets members of `punktfunk-update` start it:
//! `punktfunk-door-on@<user>.service` from the owner's own host, `punktfunk-door-off@<user>.service`
//! from the door, which knows the owner from its seat ledger. The request returns once systemd has
//! the job; the console polls `GET /host` for `door`, and reads `door_unavailable` before it asks.

use crate::update::detect::InstallKind;
use axum::http::StatusCode;

/// A refusal: the status and its sentence for the operator.
pub(crate) type Refusal = (StatusCode, String);

/// What a request to switch the door found.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
pub(crate) enum Outcome {
    /// The door was already as asked.
    Already,
    /// The switch is under way.
    Started,
}

#[cfg(not(target_os = "linux"))]
const LINUX_ONLY: &str = "Reachable without logging in is a Linux setting.";

/// Turns the door on (`on`) or off. Blocking: it runs `id` and `systemctl`.
pub(crate) fn change(on: bool) -> Result<Outcome, Refusal> {
    #[cfg(target_os = "linux")]
    {
        linux::change(on)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = on;
        Err((StatusCode::CONFLICT, LINUX_ONLY.into()))
    }
}

/// Why turning the door on would be refused right now, as the operator's next move. `None` when
/// it would start, and on the door itself. Blocking: it runs `id`.
pub(crate) fn unavailable() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        linux::unavailable()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(LINUX_ONLY.into())
    }
}

/// What to do on an install whose door files are missing. Each channel gets them its own way.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn missing(kind: InstallKind) -> String {
    let package = |command: &str| {
        format!("This install is missing the punktfunk-seats package. Install it: {command}")
    };
    match kind {
        InstallKind::Apt => package("sudo apt install punktfunk-seats"),
        InstallKind::Dnf => package("sudo dnf install punktfunk-seats"),
        InstallKind::Pacman => package("sudo pacman -S punktfunk-seats"),
        InstallKind::RpmOstree => "This install is missing the punktfunk-seats package. Update \
                                   Punktfunk, then restart: sudo /usr/share/punktfunk/update-punktfunk.sh"
            .into(),
        InstallKind::Sysext => {
            "This image doesn't include it yet. Update Punktfunk: sudo punktfunk-sysext update".into()
        }
        InstallKind::Nix => "Turn it on in your NixOS configuration with \
                             services.punktfunk.seats.enable = true, then rebuild."
            .into(),
        InstallKind::SteamosSource => "This Deck hasn't set it up yet. Run bash \
                                       ~/punktfunk/scripts/steamdeck/update.sh in Konsole and \
                                       enter your password."
            .into(),
        InstallKind::Source | InstallKind::Flatpak | InstallKind::WindowsInstaller => {
            "This build doesn't include it. Install Punktfunk from a package to use it.".into()
        }
    }
}

/// The unit that switches the door for `user`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn unit(on: bool, user: &str) -> String {
    format!(
        "punktfunk-door-{}@{user}.service",
        if on { "on" } else { "off" }
    )
}

/// A user name `systemctl` takes as an instance and the helper accepts: lowercase letters,
/// digits, `_`, `-` and `.`, starting with a letter or `_`. Anything else is not started.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn instance_safe(user: &str) -> bool {
    let mut bytes = user.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
        })
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    /// The templates the helper units are installed as: a package's, or an `/etc` copy (NixOS,
    /// the SteamOS on-device build).
    const TEMPLATES: [&str; 2] = [
        "/usr/lib/systemd/system/punktfunk-door-on@.service",
        "/etc/systemd/system/punktfunk-door-on@.service",
    ];

    pub(super) fn change(on: bool) -> Result<Outcome, Refusal> {
        if on == pf_paths::seat::role().is_door() {
            return Ok(Outcome::Already);
        }
        let user = if on { own_user() } else { owner_account() }?;
        if let Some(refusal) = refusal(on, &user) {
            return Err(refusal);
        }
        start(&unit(on, &user))?;
        Ok(Outcome::Started)
    }

    pub(super) fn unavailable() -> Option<String> {
        if pf_paths::seat::role().is_door() {
            return None;
        }
        match own_user() {
            Ok(user) => refusal(true, &user),
            Err(refusal) => Some(refusal),
        }
        .map(|(_, sentence)| sentence)
    }

    /// What stops switching the door for `user`, checked in the order the operator fixes it.
    fn refusal(on: bool, user: &str) -> Option<Refusal> {
        if !TEMPLATES.iter().any(|p| Path::new(p).exists()) {
            return Some((
                StatusCode::CONFLICT,
                missing(crate::update::detect::detect().0),
            ));
        }
        if !instance_safe(user) {
            return Some((
                StatusCode::CONFLICT,
                "This account's name can't be switched automatically.".into(),
            ));
        }
        if on && !in_update_group(Command::new("id").args(["-nG", user])) {
            return Some((
                StatusCode::FORBIDDEN,
                format!(
                    "Add {user} to the punktfunk-update group first: sudo usermod -aG punktfunk-update {user}"
                ),
            ));
        }
        if on && !in_update_group(Command::new("id").arg("-nG")) {
            return Some((
                StatusCode::FORBIDDEN,
                format!(
                    "Restart the computer to finish adding {user} to the punktfunk-update group. The host started before it."
                ),
            ));
        }
        None
    }

    /// The user this host runs as.
    fn own_user() -> Result<String, Refusal> {
        capture(Command::new("id").arg("-un"))
            .map(|s| s.trim().to_string())
            .ok_or_else(|| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Couldn't tell which account this host runs as.".into(),
                )
            })
    }

    /// The owner's account, from the seat ledger. The door has no other way to know whose
    /// files to hand back.
    fn owner_account() -> Result<String, Refusal> {
        crate::seats::snapshot()
            .owner()
            .map(|s| s.account.clone())
            .ok_or((
                StatusCode::CONFLICT,
                "This host has no owner to hand its files back to.".into(),
            ))
    }

    /// `punktfunk-update` among the groups `id -nG` prints: the user database's for a named user,
    /// this process's for none. polkit judges the process, whose groups are fixed when the user
    /// manager starts, so a fresh `usermod` counts only after a restart.
    fn in_update_group(id: &mut Command) -> bool {
        capture(id).is_some_and(|groups| groups.split_whitespace().any(|g| g == "punktfunk-update"))
    }

    fn capture(cmd: &mut Command) -> Option<String> {
        let out = cmd.output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `systemctl start --no-block`: polkit decides, and the job runs on after this returns.
    /// `--no-ask-password`: a refusal answers at once instead of a password prompt on the box's
    /// own desktop that nobody streaming sees.
    fn start(unit: &str) -> Result<(), Refusal> {
        let out = Command::new("systemctl")
            .args(["start", "--no-block", "--no-ask-password", unit])
            .output()
            .map_err(|e| {
                tracing::warn!(%unit, error = %e, "door switch did not launch");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Couldn't start the switch. Check that systemd is running.".to_string(),
                )
            })?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        tracing::warn!(%unit, %stderr, "door switch refused");
        let said = stderr.to_ascii_lowercase();
        let denied = said.contains("access denied")
            || said.contains("interactive authentication")
            || said.contains("permission denied");
        Err(if denied {
            (
                StatusCode::FORBIDDEN,
                "This account isn't allowed to switch it. Add it to the punktfunk-update group, then try again."
                    .to_string(),
            )
        } else {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Couldn't start the switch. Check the host log.".to_string(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_names_the_user_and_the_direction() {
        assert_eq!(unit(true, "enrico"), "punktfunk-door-on@enrico.service");
        assert_eq!(unit(false, "j.doe"), "punktfunk-door-off@j.doe.service");
    }

    /// The instance is data the helper re-checks, but nothing odd reaches `systemctl`: a user
    /// name with a slash, a space, an `@` or a leading dash is not started.
    #[test]
    fn only_a_plain_user_name_is_started() {
        for good in ["enrico", "_svc", "a1", "j.doe", "a-b_c"] {
            assert!(instance_safe(good), "{good}");
        }
        for bad in [
            "", "Enrico", "1abc", "-x", "a b", "a/b", "a@b", "a\u{e9}", ".x",
        ] {
            assert!(!instance_safe(bad), "{bad:?}");
        }
    }

    /// A missing door names the fix for the channel the box came from, never "update" where an
    /// update would not bring it.
    #[test]
    fn a_missing_door_names_the_fix_for_its_channel() {
        for (kind, fix) in [
            (InstallKind::Apt, "sudo apt install punktfunk-seats"),
            (InstallKind::Dnf, "sudo dnf install punktfunk-seats"),
            (InstallKind::Pacman, "sudo pacman -S punktfunk-seats"),
            (InstallKind::RpmOstree, "update-punktfunk.sh"),
            (InstallKind::Sysext, "sudo punktfunk-sysext update"),
            (InstallKind::Nix, "services.punktfunk.seats.enable = true"),
            (InstallKind::SteamosSource, "scripts/steamdeck/update.sh"),
            (InstallKind::Source, "from a package"),
        ] {
            let sentence = missing(kind);
            assert!(sentence.contains(fix), "{kind:?}: {sentence}");
            assert!(!sentence.contains("  "), "{kind:?}: {sentence}");
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn off_linux_the_switch_is_a_plain_conflict() {
        let (status, _) = change(true).unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(unavailable().as_deref(), Some(LINUX_ONLY));
    }
}
