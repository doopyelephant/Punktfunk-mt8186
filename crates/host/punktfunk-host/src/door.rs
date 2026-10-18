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
use pf_seats::model::MAX_ACCOUNT_LEN;

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

/// A user name the seat supervisor adopts as the owner, which the helper checks the same way:
/// lowercase letters, digits, `_` and `-`, starting with a letter or `_`, at most
/// [`MAX_ACCOUNT_LEN`] long. Anything else is not started.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn instance_safe(user: &str) -> bool {
    let mut bytes = user.bytes();
    user.len() <= MAX_ACCOUNT_LEN
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// Whether `id -u` printed an ordinary account's uid, the range the helper switches. A service
/// account and a `systemd-homed` user (60001 and up) are refused there with nothing changed.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn ordinary_uid(printed: &str) -> bool {
    printed
        .trim()
        .parse::<u32>()
        .is_ok_and(|uid| (1000..60000).contains(&uid))
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
        assert_eq!(unit(false, "j_doe"), "punktfunk-door-off@j_doe.service");
    }

    /// The instance is data the helper re-checks, but nothing odd reaches `systemctl`: a user
    /// name with a slash, a space, an `@` or a leading dash is not started. The supervisor
    /// refuses a `.` and a name past [`MAX_ACCOUNT_LEN`], so neither starts a switch it would
    /// abandon half way.
    #[test]
    fn only_a_plain_user_name_is_started() {
        let longest = "a".repeat(MAX_ACCOUNT_LEN);
        for good in ["enrico", "_svc", "a1", "a-b_c", longest.as_str()] {
            assert!(instance_safe(good), "{good}");
        }
        let too_long = "a".repeat(MAX_ACCOUNT_LEN + 1);
        for bad in [
            "",
            "Enrico",
            "1abc",
            "-x",
            "a b",
            "a/b",
            "a@b",
            "a\u{e9}",
            ".x",
            "j.doe",
            too_long.as_str(),
        ] {
            assert!(!instance_safe(bad), "{bad:?}");
        }
    }

    /// The helper switches ordinary accounts only; the console says so before it asks.
    #[test]
    fn only_an_ordinary_uid_is_switched() {
        for good in ["1000\n", "1001", "59999\n"] {
            assert!(ordinary_uid(good), "{good:?}");
        }
        for bad in ["", "root", "0\n", "999", "60000", "60001", "-1"] {
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

/// The key the door's console also accepts a console handoff under: the owner's host's token,
/// which the owner's own `ctl console-url` signs with. Never a contract seat's, whose user can
/// read their own. Kept level with the owner's row for the door's lifetime; absent without one.
#[cfg(target_os = "linux")]
pub(crate) const OWNER_KEY_FILE: &str = "owner-handoff-key";

/// Keeps [`OWNER_KEY_FILE`] in the door's directory level with the owner's row's token.
#[cfg(target_os = "linux")]
pub(crate) fn publish_owner_key() {
    let spawned = std::thread::Builder::new()
        .name("door-owner-key".into())
        .spawn(|| {
            let path = pf_paths::config_dir().join(OWNER_KEY_FILE);
            let mut last: Option<String> = None;
            loop {
                let token = crate::seats::ledger()
                    .owner()
                    .and_then(crate::seats::token_of);
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

/// The door keeps no display policy or hooks of its own. Any its directory holds are the owner's
/// console edits: once the owner's host answers, each per-device overlay it lacks and each hook
/// it lacks goes there, and the door's file is set aside. Runs for the door's lifetime, until
/// nothing is left.
#[cfg(target_os = "linux")]
pub(crate) fn hand_over_leftovers() {
    let spawned = std::thread::Builder::new()
        .name("door-leftovers".into())
        .spawn(|| {
            let dir = pf_paths::config_dir();
            while !hand_over(&dir) {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "door leftovers not handed over");
    }
}

/// One try at [`hand_over_leftovers`]: `true` once nothing is left to hand over.
#[cfg(target_os = "linux")]
fn hand_over(dir: &std::path::Path) -> bool {
    let display = dir.join("display-settings.json");
    let hooks = dir.join("hooks.json");
    if !display.exists() && !hooks.exists() {
        return true;
    }
    let ledger = crate::seats::ledger();
    let Some(owner) = ledger.owner() else {
        return false;
    };
    if owner.runtime.state != pf_seats::RuntimeState::Running {
        return false;
    }
    let call = |method: &str, target: String, body: Option<Vec<u8>>| {
        let mut call = crate::mgmt::owner::Forwarded::get(target);
        call.method = method.into();
        if let Some(body) = body {
            call.content_type = Some("application/json".into());
            call.body = body;
        }
        match crate::seats::forward(owner, call)? {
            crate::mgmt::owner::Answer::Reply(status, _, body) if (200..300).contains(&status) => {
                Some(body)
            }
            _ => None,
        }
    };
    if display.exists() {
        let leftover = read_json(&display);
        let overlays = leftover["clients"].as_object().cloned().unwrap_or_default();
        for (fp, overlay) in overlays {
            let target = format!("display/clients/{fp}");
            let Some(have) = call("GET", target.clone(), None) else {
                return false;
            };
            let have: serde_json::Value = serde_json::from_slice(&have).unwrap_or_default();
            if have.as_object().is_some_and(|o| !o.is_empty()) {
                continue;
            }
            if call("PUT", target, Some(overlay.to_string().into_bytes())).is_none() {
                return false;
            }
        }
        set_aside(&display);
    }
    if hooks.exists() {
        let leftover: crate::hooks::HooksConfig =
            serde_json::from_value(read_json(&hooks)).unwrap_or_default();
        let Some(have) = call("GET", "hooks".into(), None) else {
            return false;
        };
        let have: crate::hooks::HooksConfig = serde_json::from_slice(&have).unwrap_or_default();
        let merged = merged_hooks(have.clone(), leftover);
        if merged.hooks.len() != have.hooks.len() {
            let body = serde_json::to_vec(&merged).unwrap_or_default();
            if call("PUT", "hooks".into(), Some(body)).is_none() {
                return false;
            }
        }
        set_aside(&hooks);
    }
    true
}

/// `have` with each hook of `extra` it lacks after it, in `extra`'s order.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn merged_hooks(
    mut have: crate::hooks::HooksConfig,
    extra: crate::hooks::HooksConfig,
) -> crate::hooks::HooksConfig {
    let same = |a: &crate::hooks::HookEntry, b: &crate::hooks::HookEntry| {
        serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
    };
    for hook in extra.hooks {
        if !have.hooks.iter().any(|h| same(h, &hook)) {
            have.hooks.push(hook);
        }
    }
    have
}

#[cfg(target_os = "linux")]
fn read_json(path: &std::path::Path) -> serde_json::Value {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// `path` renamed beside itself, so the door never hands it over twice and nothing is lost.
#[cfg(target_os = "linux")]
fn set_aside(path: &std::path::Path) {
    let mut aside = path.as_os_str().to_owned();
    aside.push(".handed-over");
    if let Err(error) = std::fs::rename(path, &aside) {
        tracing::warn!(path = %path.display(), %error, "door leftover not set aside");
    }
}

#[cfg(test)]
mod leftover_tests {
    use super::merged_hooks;
    use crate::hooks::HooksConfig;

    /// The owner's hooks stay first; the door's add only what the owner lacks.
    #[test]
    fn the_door_s_hooks_join_the_owner_s_without_duplicates() {
        let parse = |v: serde_json::Value| serde_json::from_value::<HooksConfig>(v).unwrap();
        let owner = parse(serde_json::json!({"hooks": [
            {"on": "stream.started", "run": "notify-send hi"},
        ]}));
        let door = parse(serde_json::json!({"hooks": [
            {"on": "stream.started", "run": "notify-send hi"},
            {"on": "pairing.pending", "webhook": "https://example.com/knock"},
        ]}));
        let merged = merged_hooks(owner, door);
        assert_eq!(merged.hooks.len(), 2);
        assert_eq!(merged.hooks[0].on, "stream.started");
        assert_eq!(merged.hooks[1].on, "pairing.pending");
    }
}
