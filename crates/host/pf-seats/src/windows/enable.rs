//! Turning seats on and off: the prerequisite checks, the seat display driver, the Remote
//! Desktop listener and its firewall scope, the seat listener, and the pinned RDP certificate.
//!
//! [`checks`] only reads. [`turn_on`] creates the Seats key and records there what it changed
//! (`ListenerWasOff`, `AllowRdpFromNetwork`, `RdpRulesOpened`); [`turn_off`] undoes exactly
//! that and deletes the
//! key last, so a failed restore can be retried. A refusal carries one plain sentence for the
//! operator; its cause goes to the log. The seat display driver stays in the driver store when
//! seats go off.

use super::util::{backend_error, WinResult};
use super::{keeper, seats_enabled, server_edition, session_wrapper, SEATS_KEY};
use crate::backend::BackendError;
use crate::ipc::Diagnostic;
use pf_paths::POWERSHELL;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_64KEY};
use winreg::RegKey;

const TERMINAL_SERVER_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Terminal Server";
const LISTENER_DENIED: &str = "fDenyTSConnections";
/// Seats key values: the listener was off before [`turn_on`], and the firewall choice.
const LISTENER_WAS_OFF: &str = "ListenerWasOff";
const RDP_FROM_NETWORK: &str = "AllowRdpFromNetwork";
/// The `Remote Desktop` rules [`turn_on`] enabled for the network, one name per line.
const RDP_RULES_OPENED: &str = "RdpRulesOpened";
/// The `Remote Desktop` firewall group by the resource id every Windows language shares.
const RDP_RULE_GROUP: &str = "@FirewallAPI.dll,-28752";
const LOOPBACK: &str = "127.0.0.0/8";
/// The seats package, with the driver binary its INF lists, in `{app}\staging\pfvdisplay`.
const PACKAGE: [&str; 3] = [
    "pf_vdisplay_seats.inf",
    "pf_vdisplay_seats.cat",
    "pf_vdisplay.dll",
];
/// The RDP listener takes a moment to come up after `fDenyTSConnections` flips.
const TRUST_ATTEMPTS: u32 = 3;
const TRUST_RETRY: Duration = Duration::from_secs(2);
/// The role's install state, then licensing and a GPU as `0`/`1` flags, on the last line. A role
/// the cmdlet can't read is `Unknown`, after the cmdlet's error. The licensing types 2 and 4 are
/// Per Device and Per User; an adapter named `Microsoft …` is the basic or remote display driver.
const PROBE: &str = "$ProgressPreference='SilentlyContinue'; \
    $ErrorActionPreference='SilentlyContinue'; \
    $role = 'Unknown'; \
    try { $state = (Get-WindowsFeature RDS-RD-Server -ErrorAction Stop).InstallState; \
    if ($state) { $role = $state } } catch { $_.Exception.Message }; \
    $mode = (Get-CimInstance -Namespace root/cimv2/TerminalServices \
    -ClassName Win32_TerminalServiceSetting).LicensingType; \
    $gpu = [bool](Get-CimInstance Win32_VideoController | \
    Where-Object { $_.Name -notlike 'Microsoft *' }); \
    '{0} {1} {2}' -f $role, [int]($mode -in 2,4), [int]$gpu";

/// The prerequisites for seats, one diagnostic each: info when met, an error with the next move
/// when not. A desktop edition runs seats only through a Remote Desktop wrapper the operator
/// installed, a warning, and has no role or licensing to ask about.
pub(super) fn checks() -> Vec<Diagnostic> {
    let mut checks = Vec::new();
    let server = match server_edition() {
        Some(true) => {
            checks.push(Diagnostic::info(
                "windows_server",
                "This is Windows Server.",
            ));
            true
        }
        Some(false) if session_wrapper() => {
            checks.push(Diagnostic::warning(
                "windows_server",
                "A Remote Desktop wrapper lets this desktop edition of Windows run seats. Only Windows Server is supported.",
            ));
            false
        }
        Some(false) => {
            checks.push(Diagnostic::error(
                "windows_server",
                "Seats need Windows Server — this is a desktop edition of Windows.",
            ));
            return checks;
        }
        None => {
            checks.push(Diagnostic::error(
                "windows_server",
                "Couldn't tell which edition of Windows this is.",
            ));
            return checks;
        }
    };
    let Some((role, [licensing, gpu])) = probe() else {
        checks.push(Diagnostic::error(
            "probe",
            "Couldn't read this machine's Remote Desktop and graphics settings.",
        ));
        return checks;
    };
    if server {
        checks.push(role_check(&role));
        checks.push(check(
            licensing,
            "rds_licensing",
            "Remote Desktop licensing is set.",
            "Remote Desktop licensing isn't set. Choose Per Device or Per User in Server Manager, then try again.",
        ));
    }
    checks.push(check(
        gpu,
        "gpu",
        "A graphics card is installed.",
        "No graphics card was found. Seats need a GPU.",
    ));
    checks
}

/// Turn seats on. A failure after the Seats key exists on a machine that had none puts the
/// machine back, so `enabled` never reports a half-done switch.
pub(super) fn turn_on(host_path: &Path, allow_rdp_from_network: bool) -> WinResult<()> {
    let fresh = !seats_enabled();
    let done = apply(host_path, allow_rdp_from_network);
    if done.is_err() && fresh {
        let _ = turn_off();
    }
    done
}

/// Restore the listener and the firewall scope `turn_on` changed, then remove the Seats key.
/// Nothing to do without the key.
pub(super) fn turn_off() -> WinResult<()> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok(seats) = hklm.open_subkey_with_flags(SEATS_KEY, KEY_READ | KEY_WOW64_64KEY) else {
        return Ok(());
    };
    close_rdp_rules(&seats)?;
    if seats.get_value::<u32, _>(RDP_FROM_NETWORK).ok() == Some(0) {
        firewall_scope("Any").map_err(|cause| {
            refuse(
                "rdp_firewall",
                "Couldn't restore Remote Desktop's firewall rules.",
                cause,
            )
        })?;
    }
    if seats.get_value::<u32, _>(LISTENER_WAS_OFF).ok() == Some(1) {
        deny_listener(true).map_err(|cause| {
            refuse(
                "rdp_listener",
                "Couldn't turn Remote Desktop back off.",
                cause,
            )
        })?;
    }
    super::seat_listener::remove();
    drop(seats);
    hklm.delete_subkey_with_flags(SEATS_KEY, KEY_WOW64_64KEY)
        .map_err(|cause| refuse("seats_key", "Couldn't turn seats off.", cause))
}

/// Seats are on and the operator let the network reach Remote Desktop.
pub(super) fn rdp_from_network() -> bool {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(SEATS_KEY, KEY_READ | KEY_WOW64_64KEY)
        .and_then(|seats| seats.get_value::<u32, _>(RDP_FROM_NETWORK))
        .is_ok_and(|allowed| allowed == 1)
}

/// A refusal as the check list shows it.
pub(super) fn refusal(error: BackendError) -> Diagnostic {
    Diagnostic::error(error.code, error.message)
}

fn apply(host_path: &Path, allow_rdp_from_network: bool) -> WinResult<()> {
    let dir = package_dir(host_path);
    if PACKAGE.iter().any(|name| !dir.join(name).is_file()) {
        return Err(refuse(
            "driver_missing",
            "The seat display driver isn't on this machine. Reinstall punktfunk, then try again.",
            dir.display(),
        ));
    }
    let (seats, _) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .create_subkey_with_flags(SEATS_KEY, KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY)
        .map_err(|cause| refuse("seats_key", "Couldn't turn seats on.", cause))?;
    install_driver(&dir, true)?;
    allow_listener(&seats)?;
    // A choice made earlier narrowed the rules; allowing the network now widens them again.
    let narrowed = seats.get_value::<u32, _>(RDP_FROM_NETWORK).ok() == Some(0);
    let scope = match (allow_rdp_from_network, narrowed) {
        (false, _) => Some(LOOPBACK),
        (true, true) => Some("Any"),
        (true, false) => None,
    };
    if let Some(scope) = scope {
        firewall_scope(scope).map_err(|cause| {
            refuse(
                "rdp_firewall",
                "Couldn't limit Remote Desktop to this machine.",
                cause,
            )
        })?;
    }
    if allow_rdp_from_network {
        open_rdp_rules(&seats)?;
    } else {
        close_rdp_rules(&seats)?;
    }
    seats
        .set_value(RDP_FROM_NETWORK, &u32::from(allow_rdp_from_network))
        .map_err(|cause| refuse("seats_key", "Couldn't turn seats on.", cause))?;
    if let Err(cause) = super::seat_listener::ensure(host_path) {
        tracing::warn!(%cause, "seat listener not ready — seats start without pad audio");
    }
    trust_rdp_certificate(host_path)
}

/// `{app}\staging\pfvdisplay`: the packages setup leaves beside the host for the console.
fn package_dir(host_path: &Path) -> PathBuf {
    host_path.with_file_name("staging").join("pfvdisplay")
}

/// Where the seats package stands against the one staged beside the host.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DriverState {
    Current,
    /// In the store, but not the staged version: an upgrade the store hasn't taken yet.
    Outdated,
    Missing,
    /// No package beside the host to compare with.
    Unstaged,
}

pub(super) fn driver_state(host_path: &Path) -> DriverState {
    let Some(staged) = driver_ver(&package_dir(host_path).join(PACKAGE[0])) else {
        return DriverState::Unstaged;
    };
    let stored = store_versions();
    if stored.contains(&staged) {
        DriverState::Current
    } else if stored.is_empty() {
        DriverState::Missing
    } else {
        DriverState::Outdated
    }
}

/// Put the staged seats package in the driver store when the store lacks that version: an
/// uninstall deletes it, and an upgrade stages a new one only beside the host. Seats running
/// now keep theirs until their next session.
pub(super) fn ensure_driver(host_path: &Path) -> WinResult<()> {
    let state = driver_state(host_path);
    if state == DriverState::Current {
        return Ok(());
    }
    tracing::info!(
        ?state,
        "seat display driver not current in the driver store — installing"
    );
    let dir = package_dir(host_path);
    if PACKAGE.iter().any(|name| !dir.join(name).is_file()) {
        return Err(refuse(
            "seat_display_driver",
            "The seat display driver isn't on this machine. Reinstall punktfunk, then try again.",
            dir.display(),
        ));
    }
    install_driver(&dir, false).map_err(|error| backend_error("seat_display_driver", error.message))
}

/// The `DriverVer` value of an INF, such as `10/12/2026,9.9.1012.257`, spaces dropped.
fn driver_ver(inf: &Path) -> Option<String> {
    let bytes = std::fs::read(inf).ok()?;
    let text = match bytes.as_slice() {
        [0xFF, 0xFE, rest @ ..] => String::from_utf16_lossy(
            &rest
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        ),
        _ => String::from_utf8_lossy(&bytes).into_owned(),
    };
    inf_driver_ver(&text)
}

fn inf_driver_ver(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        key.trim().eq_ignore_ascii_case("DriverVer").then(|| {
            let value = value.split(';').next().unwrap_or_default();
            value.chars().filter(|c| !c.is_whitespace()).collect()
        })
    })
}

/// The `DriverVer` of every seats package in the driver store, one folder each.
fn store_versions() -> Vec<String> {
    let repository = pf_paths::system32(r"DriverStore\FileRepository");
    let Ok(entries) = std::fs::read_dir(repository) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with("pf_vdisplay_seats.inf_")
        })
        .filter_map(|entry| driver_ver(&entry.path().join(PACKAGE[0])))
        .collect()
}

/// Whether Windows' own Remote Desktop display driver is dated after ours: the newer date wins
/// the seat's display, and a seat on Microsoft's never streams. `None` when either is unreadable.
pub(super) fn inbox_display_driver_newer(host_path: &Path) -> Option<bool> {
    let ours = driver_ver(&package_dir(host_path).join(PACKAGE[0]))?;
    let windows = driver_ver(Path::new(&pf_paths::windows_dir(r"INF\rdpidd.inf")))?;
    Some(ver_date(&windows)? >= ver_date(&ours)?)
}

/// `mm/dd/yyyy,…` as `(yyyy, mm, dd)`, so dates compare in order.
fn ver_date(driver_ver: &str) -> Option<(u32, u32, u32)> {
    let date = driver_ver.split(',').next()?;
    let mut parts = date.split('/').map(|part| part.parse::<u32>().ok());
    let (month, day, year) = (parts.next()??, parts.next()??, parts.next()??);
    Some((year, month, day))
}

/// Add the seats package to the driver store. The terminal-services stack makes the devnode
/// when a seat's session starts and picks the newest package then, so none is created here.
/// `install` also moves live devnodes onto it, which restarts a running seat's display: only
/// turning seats on asks for that. 3010 is a driver added that a restart would load.
fn install_driver(dir: &Path, install: bool) -> WinResult<()> {
    let inf = dir.join(PACKAGE[0]);
    let inf = inf.to_string_lossy();
    let args: &[&str] = if install {
        &["/add-driver", &inf, "/install"]
    } else {
        &["/add-driver", &inf]
    };
    match run("pnputil.exe", args) {
        Some(0 | 3010) => Ok(()),
        code => Err(refuse(
            "driver_install",
            "Windows couldn't install the seat display driver.",
            format!("pnputil exited {code:?}"),
        )),
    }
}

/// Turn the Remote Desktop listener on, noting under the Seats key that it was off.
fn allow_listener(seats: &RegKey) -> WinResult<()> {
    let listener =
        |cause: std::io::Error| refuse("rdp_listener", "Couldn't turn on Remote Desktop.", cause);
    let terminal_server = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            TERMINAL_SERVER_KEY,
            KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY,
        )
        .map_err(listener)?;
    if terminal_server.get_value::<u32, _>(LISTENER_DENIED).ok() == Some(1) {
        // The note comes first: a crash between the two writes still restores the listener.
        seats
            .set_value(LISTENER_WAS_OFF, &1_u32)
            .map_err(listener)?;
        deny_listener(false).map_err(listener)?;
    }
    Ok(())
}

fn deny_listener(deny: bool) -> std::io::Result<()> {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(TERMINAL_SERVER_KEY, KEY_SET_VALUE | KEY_WOW64_64KEY)?
        .set_value(LISTENER_DENIED, &u32::from(deny))
}

/// Scope the `Remote Desktop` rule group to `remote`: an address range or `Any`.
fn firewall_scope(remote: &str) -> Result<(), String> {
    let script = format!(
        "$ErrorActionPreference='Stop'; \
         Set-NetFirewallRule -Group '{RDP_RULE_GROUP}' -RemoteAddress {remote}"
    );
    match run(POWERSHELL, &powershell_args(&script)) {
        Some(0) => Ok(()),
        code => Err(format!("Set-NetFirewallRule exited {code:?}")),
    }
}

/// Enable the `Remote Desktop` rules that are off, for an operator who lets the network reach
/// Remote Desktop. Turning the listener on by registry enables none. The names are noted first,
/// so a crash between the two still closes them.
fn open_rdp_rules(seats: &RegKey) -> WinResult<()> {
    let fail = |cause: String| {
        refuse(
            "rdp_firewall",
            "Couldn't open Remote Desktop to the network.",
            cause,
        )
    };
    let off = powershell_lines(&format!(
        "$ErrorActionPreference='Stop'; \
         Get-NetFirewallRule -Group '{RDP_RULE_GROUP}' | \
         Where-Object {{ $_.Enabled -eq 'False' }} | ForEach-Object {{ $_.Name }}"
    ))
    .map_err(fail)?;
    if off.is_empty() {
        return Ok(());
    }
    let mut opened = rules_opened(seats);
    opened.extend(off.iter().cloned());
    opened.sort();
    opened.dedup();
    seats
        .set_value(RDP_RULES_OPENED, &opened.join("\n"))
        .map_err(|cause| fail(cause.to_string()))?;
    powershell_lines(&format!(
        "$ErrorActionPreference='Stop'; Enable-NetFirewallRule -Name {}",
        ps_list(&off)
    ))
    .map(drop)
    .map_err(fail)
}

/// Disable the rules [`open_rdp_rules`] enabled, then forget them. A rule since removed is
/// skipped.
fn close_rdp_rules(seats: &RegKey) -> WinResult<()> {
    let opened = rules_opened(seats);
    if opened.is_empty() {
        return Ok(());
    }
    let fail = |cause: String| {
        refuse(
            "rdp_firewall",
            "Couldn't close Remote Desktop to the network.",
            cause,
        )
    };
    powershell_lines(&format!(
        "$ErrorActionPreference='Stop'; \
         Get-NetFirewallRule -Name {} -ErrorAction SilentlyContinue | Disable-NetFirewallRule",
        ps_list(&opened)
    ))
    .map_err(fail)?;
    match seats.delete_value(RDP_RULES_OPENED) {
        Err(cause) if cause.kind() != std::io::ErrorKind::NotFound => Err(fail(cause.to_string())),
        _ => Ok(()),
    }
}

fn rules_opened(seats: &RegKey) -> Vec<String> {
    seats
        .get_value::<String, _>(RDP_RULES_OPENED)
        .map(|names| names.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// `names` as a PowerShell string array, each single-quoted.
fn ps_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("'{}'", name.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",")
}

/// A PowerShell script's non-empty output lines. `Err` when it fails or doesn't launch.
fn powershell_lines(script: &str) -> Result<Vec<String>, String> {
    let output = Command::new(pf_paths::system32(POWERSHELL))
        .args(powershell_args(script))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("powershell exited {:?}", output.status.code()));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Record the leaf TermService presents. The keeper observes it, since the handshake needs
/// IronRDP, which the service doesn't link.
fn trust_rdp_certificate(host_path: &Path) -> WinResult<()> {
    let keeper = keeper::keeper_path(host_path);
    if !keeper.is_file() {
        return Err(refuse(
            "keeper_missing",
            "A punktfunk file is missing. Reinstall punktfunk, then try again.",
            keeper.display(),
        ));
    }
    let mut cause = String::new();
    for attempt in 0..TRUST_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(TRUST_RETRY);
        }
        match Command::new(&keeper)
            .arg("trust")
            .stdin(Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => cause = String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            Err(error) => cause = error.to_string(),
        }
    }
    Err(refuse(
        "rdp_trust",
        "Couldn't record this machine's Remote Desktop certificate. Check that Remote Desktop is running, then try again.",
        cause,
    ))
}

fn probe() -> Option<(String, [bool; 2])> {
    let output = Command::new(pf_paths::system32(POWERSHELL))
        .args(powershell_args(PROBE))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    tracing::debug!(output = %text.trim(), "seat prerequisite probe");
    parse_probe(&text)
}

/// The last non-empty line: the role's install state, then two `0`/`1` flags.
fn parse_probe(text: &str) -> Option<(String, [bool; 2])> {
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let mut fields = line.split_whitespace();
    let role = fields.next()?.to_string();
    let flags: Vec<bool> = fields.map(|flag| flag == "1").collect();
    Some((role, flags.try_into().ok()?))
}

/// The Session Host role by its `Get-WindowsFeature` install state. A role the probe can't read
/// warns instead of refusing, so a broken cmdlet never blocks a machine that has the role.
fn role_check(state: &str) -> Diagnostic {
    const CODE: &str = "rds_role";
    match state {
        "Installed" => Diagnostic::info(CODE, "Remote Desktop Session Host is installed."),
        "Unknown" => Diagnostic::warning(
            CODE,
            "Couldn't confirm that Remote Desktop Session Host is installed. If it isn't, add it under Remote Desktop Services in Server Manager.",
        ),
        _ => Diagnostic::error(
            CODE,
            "Remote Desktop Session Host isn't installed. Add it under Remote Desktop Services in Server Manager, restart, then try again.",
        ),
    }
}

fn powershell_args(script: &str) -> [&str; 6] {
    [
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        script,
    ]
}

/// A System32 tool's exit code, output discarded. `None` when it did not launch.
pub(super) fn run(tool: &str, args: &[&str]) -> Option<i32> {
    Command::new(pf_paths::system32(tool))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?
        .code()
}

fn check(ok: bool, code: &str, met: &str, unmet: &str) -> Diagnostic {
    if ok {
        Diagnostic::info(code, met)
    } else {
        Diagnostic::error(code, unmet)
    }
}

/// A refusal the operator reads: `sentence` is the answer, `cause` goes to the log.
fn refuse(code: &str, sentence: &str, cause: impl std::fmt::Display) -> BackendError {
    tracing::warn!(code, %cause, "seat change refused");
    backend_error(code, sentence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::DiagnosticLevel;

    #[test]
    fn the_probe_answers_on_its_last_line() {
        let installed = ("Installed".to_string(), [false, true]);
        assert_eq!(parse_probe("Installed 0 1\r\n"), Some(installed));
        let unread = ("Unknown".to_string(), [true; 2]);
        assert_eq!(
            parse_probe("Access is denied.\r\nUnknown 1 1\r\n\r\n"),
            Some(unread)
        );
        assert_eq!(parse_probe("Installed 1"), None);
        assert_eq!(parse_probe(""), None);
    }

    #[test]
    fn an_infs_driver_ver_reads_without_spaces_or_comment() {
        let inf = "[Version]\r\nSignature=\"$Windows NT$\"\r\nDriverVer = 10/12/2026, 9.9.1012.257 ; build\r\n";
        assert_eq!(
            inf_driver_ver(inf).as_deref(),
            Some("10/12/2026,9.9.1012.257")
        );
        assert_eq!(inf_driver_ver("[Version]\r\n"), None);
    }

    /// The newer date wins the seat's display, so dates compare year first.
    #[test]
    fn driver_dates_compare_year_first() {
        let ours = ver_date("10/12/2026,9.9.1012.257").unwrap();
        let windows = ver_date("06/21/2006,10.0.26100.7309").unwrap();
        assert!(windows < ours);
        assert!(ver_date("12/31/2025,1.0") < ver_date("01/01/2026,1.0"));
        assert_eq!(ver_date("not a date"), None);
    }

    #[test]
    fn only_an_absent_role_reads_as_not_installed() {
        assert_eq!(role_check("Installed").level, DiagnosticLevel::Info);
        assert_eq!(role_check("Unknown").level, DiagnosticLevel::Warning);
        assert_eq!(role_check("Available").level, DiagnosticLevel::Error);
        assert_eq!(role_check("Removed").level, DiagnosticLevel::Error);
    }

    #[test]
    fn rule_names_reach_powershell_quoted() {
        let names = [
            "RemoteDesktop-UserMode-In-TCP".to_string(),
            "it's".to_string(),
        ];
        assert_eq!(ps_list(&names), "'RemoteDesktop-UserMode-In-TCP','it''s'");
    }
}
