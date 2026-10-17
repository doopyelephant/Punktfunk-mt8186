//! Production Windows implementation of the seat platform boundary.
//!
//! Account, credential, WTS, process/job, RDP and supervisor concerns are split
//! into narrow modules. `WindowsBackend` runs inside the punktfunk service and
//! owns only per-instance state. It opens one hardened secret root, takes the
//! host executable from its caller, and delegates each portable command. The
//! `HKLM\SOFTWARE\Punktfunk\Seats` key is the switch, which `enable` writes
//! and removes: without it no seat starts, and a turn-on that did not finish leaves
//! it marked `Enabling`, which reads as off. Doctor reports prerequisite and ownership evidence
//! but never infers HDR or IDD health from static configuration.

mod accounts;
mod credentials;
mod enable;
pub mod keeper;
pub mod pipe;
mod process;
mod rdp;
mod seat_listener;
mod steam;
mod supervisor;
mod util;
mod wts;

use crate::backend::{BackendError, PlatformBackend};
use crate::ipc::{Diagnostic, DiagnosticLevel, SeatingStatus};
use crate::model::{
    Ledger, RuntimeState, RuntimeStatus, Seat, DEFAULT_MGMT_PORTS, DEFAULT_NATIVE_PORTS,
};
use crate::persistence::SecretRoot;
use accounts::AccountManager;
pub use accounts::{seats_group_sid, SEATS_GROUP};
use credentials::CredentialStore;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use supervisor::Supervisor;
use util::{backend_error, io_error, require_local_system, WinResult};
use util::{port_open, udp_port_owners};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::System::SystemInformation::{
    IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64, OSVERSIONINFOW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_WOW64_64KEY};
use winreg::RegKey;

/// Whose existence reserves display slots 12 through 15, and turns seats on unless it holds
/// [`ENABLING`].
const SEATS_KEY: &str = r"SOFTWARE\Punktfunk\Seats";
/// How long the boot warm-up waits for TermService, and how often it looks.
const TERMSERVICE_WAIT: Duration = Duration::from_secs(600);
const TERMSERVICE_POLL: Duration = Duration::from_secs(2);
/// A value of the Seats key while a turn-on is unfinished. A key without it is a complete one,
/// which older builds left too.
const ENABLING: &str = "Enabling";

#[derive(Clone)]
pub struct WindowsBackend {
    root: SecretRoot,
    host_path: PathBuf,
    accounts: AccountManager,
    supervisor: Supervisor,
    /// Set by a stop, cleared by the next start: a start reads it between its steps.
    stopped: Arc<AtomicBool>,
}

/// Whether seats are on: the Seats key exists and no turn-on is unfinished, read through the
/// 64-bit registry view.
pub fn seats_enabled() -> bool {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(SEATS_KEY, KEY_QUERY_VALUE | KEY_WOW64_64KEY)
        .is_ok_and(|seats| finished(&seats))
}

/// A Seats key no turn-on is still filling: the ones older builds left have no `Enabling`.
fn finished(seats: &RegKey) -> bool {
    seats.get_value::<u32, _>(ENABLING).is_err()
}

/// Turn seats off without the supervisor, for the uninstaller once the service is gone: Remote
/// Desktop's listener and firewall scope go back as they were. Seat accounts stay.
pub fn turn_off() -> Result<(), BackendError> {
    enable::turn_off()
}

/// Whether this is a Windows Server edition (`ProductType` other than `WinNT`), which
/// allows concurrent RDP sessions without a session provider. `None` when unreadable.
pub fn server_edition() -> Option<bool> {
    let product: String = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            r"SYSTEM\CurrentControlSet\Control\ProductOptions",
            KEY_QUERY_VALUE | KEY_WOW64_64KEY,
        )
        .ok()?
        .get_value("ProductType")
        .ok()?;
    Some(!product.eq_ignore_ascii_case("WinNT"))
}

/// Whether TermService loads a DLL other than `termsrv.dll`: a Remote Desktop wrapper the
/// operator installed, which lets a desktop edition run a session per seat. We ship none.
pub fn session_wrapper() -> bool {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            r"SYSTEM\CurrentControlSet\Services\TermService\Parameters",
            KEY_QUERY_VALUE | KEY_WOW64_64KEY,
        )
        .and_then(|key| key.get_value::<String, _>("ServiceDll"))
        .is_ok_and(|dll| wraps_termsrv(&dll))
}

/// `ServiceDll` names a DLL other than `termsrv.dll`. An in-place patch of `termsrv.dll` keeps
/// the stock name, so it reads as no wrapper.
fn wraps_termsrv(service_dll: &str) -> bool {
    let name = service_dll
        .trim()
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or_default();
    !name.is_empty() && !name.eq_ignore_ascii_case("termsrv.dll")
}

fn termservice_running() -> WinResult<bool> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| io_error("termservice", "open Service Control Manager", error))?;
    let service = manager
        .open_service("TermService", ServiceAccess::QUERY_STATUS)
        .map_err(|error| io_error("termservice", "open TermService", error))?;
    let status = service
        .query_status()
        .map_err(|error| io_error("termservice", "query TermService", error))?;
    Ok(status.current_state == ServiceState::Running)
}

impl WindowsBackend {
    /// `host_path` is the absolute `punktfunk-host.exe` each seat runs, the service's own.
    pub fn open(root: impl AsRef<Path>, host_path: PathBuf) -> Result<Self, BackendError> {
        if !host_path.is_absolute() {
            return Err(backend_error(
                "host_path_invalid",
                "host executable path is not absolute",
            ));
        }
        let root = SecretRoot::open(root)
            .map_err(|error| io_error("seat_root", "open hardened seat root", error))?;
        let credentials = CredentialStore::new(root.clone());
        let accounts = AccountManager::new(credentials);
        let supervisor = Supervisor::new(root.clone(), accounts.clone(), host_path.clone());
        Ok(Self {
            root,
            host_path,
            accounts,
            supervisor,
            stopped: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn stop_all(&self) {
        self.supervisor.stop_all();
    }

    /// Narrows Remote Desktop to this PC again where the operator chose that and a firewall
    /// rule has opened it since. For the service's start.
    pub fn keep_rdp_local(&self) {
        enable::keep_rdp_local();
    }

    /// With seats on, waits until TermService runs, `stop` is set or [`TERMSERVICE_WAIT`] is
    /// up. The service starts at boot without waiting for it, and no seat starts before it.
    pub fn wait_for_termservice(&self, stop: &AtomicBool) {
        let deadline = Instant::now() + TERMSERVICE_WAIT;
        while seats_enabled()
            && !stop.load(Ordering::SeqCst)
            && !matches!(termservice_running(), Ok(true))
            && Instant::now() < deadline
        {
            std::thread::sleep(TERMSERVICE_POLL);
        }
    }

    /// Ends the start in flight once a stop has reached it, between two of its steps. The
    /// service tells the start's caller the seat was stopped.
    fn unless_stopped(&self) -> WinResult<()> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(backend_error(
                "seat_stopped",
                "the seat was stopped while it started",
            ));
        }
        Ok(())
    }

    fn ensure_runtime_prerequisites(&self) -> WinResult<()> {
        require_local_system()?;
        if !seats_enabled() {
            return Err(backend_error(
                "seats_off",
                r"HKLM\SOFTWARE\Punktfunk\Seats is missing or unfinished; seats are off on this host",
            ));
        }
        if !termservice_running()? {
            return Err(backend_error(
                "termservice_stopped",
                "TermService is not running",
            ));
        }
        Ok(())
    }
}

impl PlatformBackend for WindowsBackend {
    fn provision(&self, seat: &Seat) -> Result<(), BackendError> {
        self.ensure_runtime_prerequisites()?;
        self.accounts.provision(seat)
    }

    fn start(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.stopped.store(false, Ordering::SeqCst);
        self.ensure_runtime_prerequisites()?;
        // An uninstall deletes the seat display driver and an upgrade only stages the new one:
        // without it the seat logs on to Microsoft's adapter and never streams.
        enable::ensure_driver(&self.host_path)?;
        self.unless_stopped()?;
        // An upgrade's new enumerator reaches the listener here. Without it the seat still
        // starts, on `RDP-Tcp`, with Remote Audio and no pad audio.
        if let Err(cause) = seat_listener::ensure(&self.host_path) {
            tracing::warn!(%cause, "seat listener not ready — seats start without pad audio");
        }
        self.unless_stopped()?;
        // The box's shared Steam shortcuts start a seat's own copy on a seat.
        match self.root.open_child_dir("temp") {
            Ok(temp) => steam::wrap_box_shortcuts(&self.host_path, temp.path()),
            Err(error) => tracing::warn!(%error, "box steam shortcuts not rewired"),
        }
        self.supervisor.start(seat)
    }

    fn stop(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.stopped.store(true, Ordering::SeqCst);
        self.supervisor.stop(seat)
    }

    /// The seat host's own directories go with the seat: its identity key, token and logs, as a
    /// Linux seat's directory does. A directory that won't go is logged; the seat is gone anyway.
    /// An account that isn't the seat's is refused before the stop logs anything off.
    fn remove(&self, seat: &Seat) -> Result<(), BackendError> {
        self.accounts.ensure_owned_or_absent(seat)?;
        let _ = self.supervisor.stop(seat)?;
        self.accounts.delete(seat)?;
        for parent in ["hosts", "temp"] {
            let dir = self
                .root
                .open_child_dir(parent)
                .and_then(|root| root.child_path(seat.id.as_str()));
            match dir.and_then(std::fs::remove_dir_all) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => tracing::warn!(%error, parent, "seat directory not removed"),
            }
        }
        Ok(())
    }

    fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        Ok(self.supervisor.status(seat).status)
    }

    /// Before TermService runs there is no session to find, and the look waits for it.
    fn reconcile(&self, seat: &Seat, _status: &RuntimeStatus) {
        if termservice_running().unwrap_or(false) {
            self.supervisor.sweep_leftovers(seat);
        }
    }

    fn doctor(&self, ledger: &Ledger) -> Result<Vec<Diagnostic>, BackendError> {
        Ok(self.doctor_report(ledger))
    }

    /// `<root>\hosts\<id>`, the `PUNKTFUNK_CONFIG_DIR` the supervisor starts the host with.
    fn host_config_dir(&self, seat: &Seat) -> Option<PathBuf> {
        Some(self.root.path().join("hosts").join(seat.id.as_str()))
    }

    fn seating(&self) -> Result<SeatingStatus, BackendError> {
        Ok(SeatingStatus {
            enabled: seats_enabled(),
            allow_rdp_from_network: enable::rdp_from_network(),
            checks: enable::checks(),
        })
    }

    fn prepare(&self) -> Vec<Diagnostic> {
        enable::prepare()
    }

    fn enable(
        &self,
        allow_rdp_from_network: bool,
        prepared: Vec<Diagnostic>,
    ) -> Result<SeatingStatus, BackendError> {
        let mut checks = enable::checks();
        // A role waiting on a restart can't take a licensing mode yet; the restart row says so.
        if prepared.iter().any(|row| row.code == "restart") {
            checks.retain(|check| check.code != "rds_licensing" && check.code != "restart");
        }
        let blocked = prepared
            .iter()
            .chain(&checks)
            .any(|check| check.level == DiagnosticLevel::Error);
        checks.splice(0..0, prepared);
        if !blocked {
            let turned_on = enable::turn_on(&self.host_path, allow_rdp_from_network);
            checks.extend(turned_on.err().map(enable::refusal));
        }
        Ok(SeatingStatus {
            enabled: seats_enabled(),
            allow_rdp_from_network: enable::rdp_from_network(),
            checks,
        })
    }

    fn disable(&self, _keep_accounts: bool) -> Result<SeatingStatus, BackendError> {
        let checks = enable::turn_off().err().map(enable::refusal).into_iter();
        Ok(SeatingStatus {
            enabled: seats_enabled(),
            allow_rdp_from_network: enable::rdp_from_network(),
            checks: checks.collect(),
        })
    }
}

impl WindowsBackend {
    /// The box's rows, then each seat's. Every row is one plain sentence; a problem names the
    /// next move. `seating` adds what turning seats on checks (role, licensing, a GPU).
    fn doctor_report(&self, ledger: &Ledger) -> Vec<Diagnostic> {
        let deadline = Instant::now() + enable::DOCTOR_BUDGET;
        let mut rows = Vec::new();
        rows.push(match windows_build() {
            Ok(build) if build >= 22621 => Diagnostic::info(
                "windows_build",
                format!("This Windows is new enough for seats (build {build})."),
            ),
            Ok(build) => Diagnostic::error(
                "windows_build",
                format!(
                    "This Windows is too old for seats (build {build}). Seats need Windows Server 2025 or newer."
                ),
            ),
            Err(error) => Diagnostic::error(
                "windows_build",
                format!("Couldn't read the Windows version — {}.", error.message),
            ),
        });
        if !matches!(native_x64(), Ok(true)) {
            rows.push(Diagnostic::error("architecture", "Seats need an x64 PC."));
        }
        if !matches!(util::is_local_system(), Ok(true)) {
            rows.push(Diagnostic::error(
                "identity",
                "punktfunk's service isn't running as Local System. Reinstall punktfunk.",
            ));
        }
        rows.push(if seats_enabled() {
            Diagnostic::info("reservation", "Seats are on.")
        } else {
            Diagnostic::error(
                "reservation",
                "Seats are off. Turn them on under Profiles → Seats.",
            )
        });
        rows.push(match server_edition() {
            Some(true) => Diagnostic::info(
                "session_provider",
                "Windows Server gives each seat a desktop of its own.",
            ),
            Some(false) if session_wrapper() => Diagnostic::warning(
                "session_provider",
                "A Remote Desktop wrapper gives each seat a desktop on this edition of Windows. \
                 Only Windows Server is supported.",
            ),
            Some(false) => Diagnostic::error(
                "session_provider",
                "This edition of Windows runs one desktop at a time, so seats can't start. \
                 Seats need Windows Server.",
            ),
            None => Diagnostic::error(
                "session_provider",
                "Couldn't tell which edition of Windows this is.",
            ),
        });
        rows.extend(
            enable::checks_until(deadline)
                .into_iter()
                .filter(|check| check.code != "windows_server"),
        );
        if server_edition() == Some(true) {
            rows.extend(enable::licensing_row(deadline));
        }
        rows.push(match termservice_running() {
            Ok(true) => Diagnostic::info("termservice", "Remote Desktop Services is running."),
            _ => Diagnostic::error(
                "termservice",
                "Remote Desktop Services isn't running. Start it under Services, or restart the PC.",
            ),
        });
        let keeper = keeper::keeper_path(&self.host_path);
        if let Some(missing) = [&self.host_path, &keeper]
            .into_iter()
            .find(|path| !path.is_file())
        {
            rows.push(Diagnostic::error(
                "host_executable",
                format!(
                    "A punktfunk file is missing ({}). Reinstall punktfunk.",
                    missing.display()
                ),
            ));
        }
        rows.push(match enable::driver_state(&self.host_path) {
            enable::DriverState::Current => {
                Diagnostic::info("seat_display_driver", "The seat display driver is installed.")
            }
            enable::DriverState::Outdated => Diagnostic::warning(
                "seat_display_driver",
                "The seat display driver is older than this punktfunk. Seats update it when they next start.",
            ),
            enable::DriverState::Missing => Diagnostic::error(
                "seat_display_driver",
                "The seat display driver isn't installed. Seats put it back when they next start; \
                 if this stays, turn seats off and on under Profiles → Seats.",
            ),
            enable::DriverState::Unstaged => Diagnostic::error(
                "seat_display_driver",
                "The seat display driver isn't on this PC. Reinstall punktfunk.",
            ),
        });
        if enable::inbox_display_driver_newer(&self.host_path) == Some(true) {
            rows.push(Diagnostic::error(
                "seat_display_rank",
                "Windows' own Remote Desktop display driver is newer than punktfunk's, so seats \
                 start without a picture. Update punktfunk.",
            ));
        }
        rows.push(match rdp::load_pin_optional(&self.root) {
            Ok(Some(pin)) => match rdp::require_live(&self.host_path, pin) {
                Ok(()) => Diagnostic::info("rdp_pin", "Remote Desktop's certificate is recorded."),
                Err(error) => Diagnostic::error("rdp_pin", error.message),
            },
            _ => Diagnostic::error(
                "rdp_pin",
                "Remote Desktop's certificate isn't recorded, so seats can't sign in. \
                 Turn seats off and on under Profiles → Seats.",
            ),
        });
        rows.push(steam_row());
        if !ledger.seats.is_empty() && !seat_sound() {
            rows.push(Diagnostic::warning(
                "seat_audio",
                "Seats stream without sound: Steam's streaming audio driver isn't on this PC. \
                 Install Steam, then restart the seats.",
            ));
        }
        for seat in &ledger.seats {
            self.doctor_seat(seat, &mut rows);
        }
        rows
    }

    fn doctor_seat(&self, seat: &Seat, rows: &mut Vec<Diagnostic>) {
        let index = usize::from(seat.display_slot.saturating_sub(12));
        if DEFAULT_NATIVE_PORTS.get(index) != Some(&seat.native_port)
            || DEFAULT_MGMT_PORTS.get(index) != Some(&seat.mgmt_port)
        {
            rows.push(seat_diagnostic(
                DiagnosticLevel::Error,
                "seat_resources",
                format!(
                    "Its display slot and ports don't match ({}, {}, {}). Remove the profile and add it again.",
                    seat.display_slot, seat.native_port, seat.mgmt_port
                ),
                seat,
            ));
        }
        rows.push(match self.accounts.inspect(seat) {
            Ok(inspection) => account_row(seat, &inspection),
            Err(error) => seat_diagnostic(
                DiagnosticLevel::Error,
                "account",
                format!(
                    "Couldn't read its Windows account {} — {}.",
                    seat.account, error.message
                ),
                seat,
            ),
        });
        let (level, message) = match (steam::box_install(), steam::seat_exe(&seat.account)) {
            (None, _) => (
                DiagnosticLevel::Info,
                "No Steam: the PC has none to copy.".into(),
            ),
            (Some(_), Some(exe)) if exe.is_file() => (
                DiagnosticLevel::Info,
                "Has its own Steam. Steam opened on this seat starts that copy.".into(),
            ),
            (Some(_), _) => (
                DiagnosticLevel::Warning,
                "Its own Steam isn't set up yet. It's copied the first time the seat starts; \
                 Steam games start on the seat once that's done."
                    .into(),
            ),
        };
        rows.push(seat_diagnostic(level, "steam_client", message, seat));
        let snapshot = self.supervisor.status(seat);
        let state = match snapshot.status.state {
            RuntimeState::Running => Some(match snapshot.session_id {
                Some(id) => format!("Running, in Windows session {id}."),
                None => "Running.".into(),
            }),
            RuntimeState::Starting => Some(
                snapshot
                    .status
                    .detail
                    .clone()
                    .unwrap_or_else(|| "Starting…".into()),
            ),
            RuntimeState::Stopped | RuntimeState::Unknown => {
                Some("Stopped. It starts when its player connects.".into())
            }
            RuntimeState::Stopping => Some("Stopping…".into()),
            // The service's own `runtime_failed` row says why.
            RuntimeState::Failed => None,
        };
        if let Some(state) = state {
            rows.push(seat_diagnostic(
                DiagnosticLevel::Info,
                "child_state",
                state,
                seat,
            ));
        }
        let running = snapshot.status.state == RuntimeState::Running;
        let native_open = !udp_port_owners(seat.native_port).is_empty();
        let mgmt_open = port_open(seat.mgmt_port);
        if running && !(native_open && mgmt_open) {
            rows.push(seat_diagnostic(
                DiagnosticLevel::Warning,
                "seat_port",
                format!(
                    "It's running, but its ports {} and {} aren't both open. Stop and start the seat.",
                    seat.native_port, seat.mgmt_port
                ),
                seat,
            ));
        } else if !running
            && snapshot.status.state != RuntimeState::Starting
            && (native_open || mgmt_open)
        {
            let port = if native_open {
                seat.native_port
            } else {
                seat.mgmt_port
            };
            rows.push(seat_diagnostic(
                DiagnosticLevel::Warning,
                "seat_port",
                format!(
                    "Another program holds its port {port}, so the seat can't start. Restart the PC."
                ),
                seat,
            ));
        }
    }
}

/// The seat's Windows account as one row: fine, or the first thing wrong with it.
fn account_row(seat: &Seat, inspection: &accounts::AccountInspection) -> Diagnostic {
    let account = &seat.account;
    let problem = if !inspection.marker_matches {
        Some(format!(
            "Its Windows account {account} is missing or isn't punktfunk's. Remove the profile and add it again."
        ))
    } else if !inspection.credential_blob {
        Some(format!(
            "punktfunk has lost the password of its Windows account {account}. Remove the profile and add it again."
        ))
    } else if inspection.administrator {
        Some(format!(
            "Its Windows account {account} is an administrator, which a seat must not be. \
             Remove it from Administrators."
        ))
    } else if !inspection.rdp_member || !inspection.deny_console || inspection.deny_remote {
        Some(format!(
            "Its Windows account {account} lost a right it needs to sign in to its seat. \
             Remove the profile and add it again."
        ))
    } else {
        None
    };
    match problem {
        Some(problem) => seat_diagnostic(DiagnosticLevel::Error, "account", problem, seat),
        None if !inspection.seats_member => seat_diagnostic(
            DiagnosticLevel::Warning,
            "account",
            format!(
                "Its Windows account {account} left the {SEATS_GROUP} group. It's put back when the seat next starts."
            ),
            seat,
        ),
        None => seat_diagnostic(
            DiagnosticLevel::Info,
            "account",
            format!("Its Windows account {account} is set up."),
            seat,
        ),
    }
}

/// Where the box's Steam is, as seats see it. Seats copy only a Steam in its usual folder.
fn steam_row() -> Diagnostic {
    if let Some(dir) = steam::box_install() {
        return Diagnostic::info(
            "steam",
            format!(
                "Steam is installed in {}. Each seat gets a copy of its own.",
                dir.display()
            ),
        );
    }
    match steam::registered_install() {
        Some(dir) => Diagnostic::warning(
            "steam",
            format!(
                "Steam is installed in {}, so seats get no Steam. Seats copy Steam only from \
                 C:\\Program Files (x86)\\Steam; reinstall it there.",
                dir.display()
            ),
        ),
        None => Diagnostic::info("steam", "Steam isn't installed, so seats have no Steam."),
    }
}

/// Steam's streaming speakers driver, which a seat's sound device binds, is on disk.
fn seat_sound() -> bool {
    std::env::var_os("CommonProgramFiles(x86)")
        .map(PathBuf::from)
        .is_some_and(|dir| {
            dir.join(r"Steam\drivers\Windows10")
                .join(if cfg!(target_arch = "aarch64") {
                    "arm64"
                } else {
                    "x64"
                })
                .join("SteamStreamingSpeakers.inf")
                .is_file()
        })
}

fn windows_build() -> WinResult<u32> {
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `version` is the initialized, correctly sized output structure required by ntdll.
    let status = unsafe { RtlGetVersion(&mut version) };
    if status.is_ok() {
        Ok(version.dwBuildNumber)
    } else {
        Err(io_error(
            "windows_build",
            "RtlGetVersion failed",
            std::io::Error::from_raw_os_error(status.0),
        ))
    }
}

fn native_x64() -> WinResult<bool> {
    let mut process_machine = IMAGE_FILE_MACHINE::default();
    let mut native_machine = IMAGE_FILE_MACHINE::default();
    // SAFETY: current-process pseudo-handle is valid and both machine outputs remain writable.
    unsafe {
        IsWow64Process2(
            GetCurrentProcess(),
            &mut process_machine,
            Some(&mut native_machine),
        )
    }
    .map_err(|error| io_error("architecture", "IsWow64Process2 failed", error))?;
    Ok(native_machine == IMAGE_FILE_MACHINE_AMD64)
}

fn seat_diagnostic(level: DiagnosticLevel, code: &str, message: String, seat: &Seat) -> Diagnostic {
    Diagnostic {
        level,
        code: code.into(),
        message,
        seat_id: Some(seat.id.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::{finished, wraps_termsrv, WindowsBackend, ENABLING};
    use crate::backend::PlatformBackend as _;
    use crate::model::{RuntimeStatus, Seat, SeatId};
    use std::path::PathBuf;
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    /// A bare key and one a turn-on finished are on; one still marked is off.
    #[test]
    fn a_key_marked_enabling_is_not_finished() {
        let path = format!(r"Software\Punktfunk\SeatsTest{}", std::process::id());
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(&path).unwrap();
        assert!(finished(&key));
        key.set_value(ENABLING, &1_u32).unwrap();
        assert!(!finished(&key));
        key.delete_value(ENABLING).unwrap();
        assert!(finished(&key));
        drop(key);
        hkcu.delete_subkey(&path).unwrap();
    }

    /// A stop ends the start in flight at its next step.
    #[test]
    fn a_stop_ends_the_start_in_flight_between_its_steps() {
        let temp = tempfile::tempdir().unwrap();
        let backend =
            WindowsBackend::open(temp.path(), PathBuf::from(r"C:\Windows\System32\cmd.exe"))
                .unwrap();
        let seat = Seat {
            id: SeatId::random(),
            name: "test".into(),
            account: "pfs-absent".into(),
            display_slot: 12,
            native_port: 9779,
            mgmt_port: 47995,
            autostart: false,
            runtime: RuntimeStatus::stopped(),
            owner: false,
            fingerprint: None,
        };
        assert!(backend.unless_stopped().is_ok());
        backend.stop(&seat).unwrap();
        assert_eq!(backend.unless_stopped().unwrap_err().code, "seat_stopped");
    }

    #[test]
    fn only_a_dll_other_than_termsrv_is_a_wrapper() {
        assert!(!wraps_termsrv(r"%SystemRoot%\System32\termsrv.dll"));
        assert!(!wraps_termsrv(r"C:\Windows\System32\TermSrv.dll "));
        assert!(!wraps_termsrv(""));
        assert!(wraps_termsrv(r"%ProgramFiles%\RDP Wrapper\TermWrap.dll"));
        assert!(wraps_termsrv(r"%ProgramFiles%\RDP Wrapper\rdpwrap.dll"));
    }
}
