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

/// Whether `id -u` printed a uid the helper switches, a [`pf_seats::model::ordinary_uid`]. The
/// helper refuses any other account with nothing changed.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn ordinary_uid(printed: &str) -> bool {
    printed
        .trim()
        .parse::<u32>()
        .is_ok_and(pf_seats::model::ordinary_uid)
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
        if !pf_seats::model::valid_owner_account(user) {
            return Some((
                StatusCode::CONFLICT,
                "This account's name can't be switched automatically.".into(),
            ));
        }
        if on
            && capture(Command::new("id").args(["-u", user])).is_some_and(|uid| !ordinary_uid(&uid))
        {
            return Some((
                StatusCode::CONFLICT,
                "This account can't be switched automatically.".into(),
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
        match crate::seats::snapshot().owner() {
            Ok(Some(owner)) => Ok(owner.account.clone()),
            Ok(None) => Err((
                StatusCode::CONFLICT,
                "This host has no owner to hand its files back to.".into(),
            )),
            Err(_) => Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "The box's seat service isn't answering. Try again in a moment.".into(),
            )),
        }
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

/// The key the door's console also accepts a console handoff under: the owner's host's token,
/// which the owner's own `ctl console-url` signs with. Never a contract seat's, whose user can
/// read their own. Kept level with the owner's row for the door's lifetime; absent without one.
#[cfg(target_os = "linux")]
pub(crate) const OWNER_KEY_FILE: &str = "owner-handoff-key";

/// Keeps [`OWNER_KEY_FILE`] in the door's directory level with the owner's row's token. A tick
/// the supervisor doesn't answer leaves the key as it is.
#[cfg(target_os = "linux")]
pub(crate) fn publish_owner_key() {
    let spawned = std::thread::Builder::new()
        .name("door-owner-key".into())
        .spawn(|| {
            let path = pf_paths::config_dir().join(OWNER_KEY_FILE);
            let mut last: Option<String> = None;
            loop {
                let ledger = crate::seats::ledger();
                let token = match ledger.owner() {
                    Ok(owner) => owner.and_then(crate::seats::token_of),
                    Err(_) => last.clone(),
                };
                if token != last {
                    let written = match &token {
                        Some(token) => pf_paths::replace_secret_file(&path, token.as_bytes()),
                        None => std::fs::remove_file(&path).or_else(|e| {
                            if e.kind() == std::io::ErrorKind::NotFound {
                                Ok(())
                            } else {
                                Err(e)
                            }
                        }),
                    };
                    match written {
                        Ok(()) => last = token,
                        Err(error) => tracing::warn!(%error, "owner's handoff key not written"),
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "owner's handoff key not kept");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_names_the_user_and_the_direction() {
        assert_eq!(unit(true, "enrico"), "punktfunk-door-on@enrico.service");
        assert_eq!(unit(false, "j_doe"), "punktfunk-door-off@j_doe.service");
        assert_eq!(
            unit(true, "jdoe@corp.example.com"),
            "punktfunk-door-on@jdoe@corp.example.com.service"
        );
    }

    /// The helper switches ordinary accounts only; the console says so before it asks.
    #[test]
    fn only_an_ordinary_uid_is_switched() {
        for good in ["1000\n", "1001", "60000\n", "65536", "1950400009\n"] {
            assert!(ordinary_uid(good), "{good:?}");
        }
        for bad in [
            "",
            "root",
            "0\n",
            "999",
            "60001",
            "65534",
            "2147483648",
            "-1",
        ] {
            assert!(!ordinary_uid(bad), "{bad:?}");
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
