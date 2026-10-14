//! Per-seat RDP, quality-gate, and host lifecycle supervision: the OS side.
//!
//! Each seat has one [`Runtime`]: its [`State`], a condition variable and a worker.
//! There is no process-global seat singleton. The worker probes what the state names,
//! feeds [`State::step`] and performs the actions it answers; `start` and `stop` feed
//! their requests into the same state and wait on it. The lifecycle, with every timeout
//! and the retry rule, is `step.rs`.
//!
//! Start blocks until the keeper's session is active, the same-session SDR quality gate
//! has passed and the host holds both ports. A cycle that ends closes the keeper's job
//! and logs off only the session it recorded. Explicit stop is idempotent and waits for
//! that teardown to finish.

use crate::bootstrap::RdpBootstrap;
use crate::model::{RuntimeStatus, Seat};
use crate::persistence::SecretRoot;
use crate::seat_step::{Action, Look, Observation, Probe, State, START_TIMEOUT};
use crate::windows::accounts::AccountManager;
use crate::windows::process::{self, ChildProcess, Job};
use crate::windows::rdp;
use crate::windows::util::{backend_error, io_error, port_open, udp_port_owners, WinResult};
use crate::windows::wts;
use rand::Rng as _;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const STOP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub(super) struct Supervisor {
    root: SecretRoot,
    accounts: AccountManager,
    host_path: PathBuf,
    runtimes: Arc<Mutex<HashMap<String, Arc<Runtime>>>>,
}

struct Runtime {
    state: Mutex<State>,
    changed: Condvar,
    join: Mutex<Option<JoinHandle<()>>>,
}

/// What `status` and Doctor read of a seat.
pub(super) struct RuntimeSnapshot {
    pub status: RuntimeStatus,
    pub session_id: Option<u32>,
}

impl Runtime {
    fn new(account: String) -> Self {
        Self {
            state: Mutex::new(State::new(account)),
            changed: Condvar::new(),
            join: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Feed one observation and wake whoever waits on the state.
    fn step(&self, observation: Observation) -> Vec<Action> {
        let actions = self.lock().step(observation, Instant::now());
        self.changed.notify_all();
        actions
    }

    fn snapshot(&self) -> RuntimeSnapshot {
        let state = self.lock();
        RuntimeSnapshot {
            status: state.status().clone(),
            session_id: state.session(),
        }
    }
}

impl Supervisor {
    pub(super) fn new(root: SecretRoot, accounts: AccountManager, host_path: PathBuf) -> Self {
        Self {
            root,
            accounts,
            host_path,
            runtimes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(super) fn start(&self, seat: &Seat) -> WinResult<RuntimeStatus> {
        if !self.host_path.is_absolute() || !self.host_path.is_file() {
            return Err(backend_error(
                "host_missing",
                format!(
                    "configured host executable '{}' is missing",
                    self.host_path.display()
                ),
            ));
        }
        let keeper = super::keeper::keeper_path(&self.host_path);
        if !keeper.is_file() {
            return Err(backend_error(
                "keeper_missing",
                format!("seat keeper '{}' is missing", keeper.display()),
            ));
        }
        let key = seat.id.to_string();
        {
            let mut runtimes = self
                .runtimes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(existing) = runtimes.get(&key) {
                let state = existing.lock();
                if !state.finished() {
                    return match state.answer() {
                        Some(Err(error)) => Err(error.clone()),
                        _ => Ok(state.status().clone()),
                    };
                }
                drop(state);
                runtimes.remove(&key);
            }
        }
        let _ = rdp::load_pin(&self.root)?;
        let _ = self.accounts.credential_for_start(seat)?;
        if let Some(session) = wts::active_for_account(&seat.account)? {
            return Err(backend_error(
                "wts_unmanaged_active",
                format!(
                    "account '{}' already has active non-console session {}",
                    seat.account, session.id
                ),
            ));
        }
        ensure_ports_free(seat)?;

        let runtime = {
            let mut runtimes = self
                .runtimes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let runtime = Arc::new(Runtime::new(seat.account.clone()));
            runtimes.insert(key.clone(), runtime.clone());
            runtime
        };
        let worker_runtime = runtime.clone();
        let rig = Rig {
            root: self.root.clone(),
            accounts: self.accounts.clone(),
            host_path: self.host_path.clone(),
            seat: seat.clone(),
            keeper: None,
            session: None,
            launch: None,
            gate: None,
            host: None,
        };
        let worker = match std::thread::Builder::new()
            .name(format!("seat-{}", seat.id))
            .spawn(move || run_worker(&worker_runtime, rig))
        {
            Ok(worker) => worker,
            Err(error) => {
                self.runtimes
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&key);
                return Err(io_error("supervisor_spawn", "spawn seat supervisor", error));
            }
        };
        *runtime
            .join
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(worker);

        let deadline = Instant::now() + START_TIMEOUT;
        let mut state = runtime.lock();
        loop {
            if let Some(answer) = state.answer() {
                return answer.clone().map(|()| state.status().clone());
            }
            let now = Instant::now();
            if now >= deadline {
                let error = state.start_timeout();
                tracing::warn!(seat = %seat.id, "{}", error.message);
                state.step(Observation::StartTimedOut, now);
                runtime.changed.notify_all();
                return Err(error);
            }
            let wait = deadline.saturating_duration_since(now);
            let (next, _) = runtime
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(|poison| poison.into_inner());
            state = next;
        }
    }

    pub(super) fn stop(&self, seat: &Seat) -> WinResult<RuntimeStatus> {
        let key = seat.id.to_string();
        let runtime = self
            .runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(&key)
            .cloned();
        let Some(runtime) = runtime else {
            wts::logoff_account_if_present(&seat.account)?;
            return Ok(RuntimeStatus::stopped());
        };
        runtime.step(Observation::Stop);
        let deadline = Instant::now() + STOP_TIMEOUT;
        let mut state = runtime.lock();
        while !state.finished() {
            let now = Instant::now();
            if now >= deadline {
                return Err(backend_error(
                    "stop_timeout",
                    format!("seat {} did not stop within 15 seconds", seat.id),
                ));
            }
            let (next, _) = runtime
                .changed
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .unwrap_or_else(|poison| poison.into_inner());
            state = next;
        }
        drop(state);
        if let Some(worker) = runtime
            .join
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            let _ = worker.join();
        }
        wts::logoff_account_if_present(&seat.account)?;
        let mut runtimes = self
            .runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if runtimes
            .get(&key)
            .is_some_and(|stored| Arc::ptr_eq(stored, &runtime))
        {
            runtimes.remove(&key);
        }
        let idle = runtimes.is_empty();
        drop(runtimes);
        if idle {
            super::steam::restore_registration();
        }
        Ok(RuntimeStatus::stopped())
    }

    pub(super) fn status(&self, seat: &Seat) -> RuntimeSnapshot {
        self.runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(seat.id.as_str())
            .map(|runtime| runtime.snapshot())
            .unwrap_or(RuntimeSnapshot {
                status: RuntimeStatus::stopped(),
                session_id: None,
            })
    }

    pub(super) fn stop_all(&self) {
        let runtimes: Vec<_> = self
            .runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
            .cloned()
            .collect();
        for runtime in &runtimes {
            runtime.step(Observation::Stop);
        }
        for runtime in runtimes {
            let deadline = Instant::now() + STOP_TIMEOUT;
            let mut state = runtime.lock();
            while !state.finished() && Instant::now() < deadline {
                let (next, _) = runtime
                    .changed
                    .wait_timeout(state, Duration::from_millis(250))
                    .unwrap_or_else(|poison| poison.into_inner());
                state = next;
            }
            let account = state.account().to_owned();
            drop(state);
            let _ = wts::logoff_account_if_present(&account);
            if runtime.lock().finished()
                && let Some(worker) = runtime
                    .join
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .take()
            {
                let _ = worker.join();
            }
        }
        let mut runtimes = self
            .runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        runtimes.retain(|_, runtime| !runtime.lock().finished());
        let idle = runtimes.is_empty();
        drop(runtimes);
        if idle {
            super::steam::restore_registration();
        }
    }
}

/// One seat's loop: probe what the state names, step, perform what it answers.
fn run_worker(runtime: &Runtime, mut rig: Rig) {
    loop {
        let look = runtime.lock().look();
        let observation = match rig.probe(look) {
            Ok(probe) => Observation::Probe(probe),
            Err(error) => Observation::Failed(error.into()),
        };
        let mut actions = VecDeque::from(runtime.step(observation));
        while let Some(action) = actions.pop_front() {
            if action == Action::Finish {
                return;
            }
            // A failed action is the next observation, and what it answers replaces the rest.
            if let Err(error) = rig.perform(action) {
                actions = runtime.step(Observation::Failed(error.into())).into();
            }
        }
    }
}

/// What a seat's worker holds from the OS. Which of them are live follows from the
/// state's phase; nothing here is read to decide what the seat does next.
struct Rig {
    root: SecretRoot,
    accounts: AccountManager,
    host_path: PathBuf,
    seat: Seat,
    /// The keeper, then the job that contains it: they drop in that order.
    keeper: Option<(ChildProcess, Job)>,
    /// The account's session, which the end of the cycle logs off.
    session: Option<u32>,
    launch: Option<Launch>,
    gate: Option<GateRun>,
    host: Option<ChildProcess>,
}

/// What a session's children start with.
struct Launch {
    host_root: SecretRoot,
    temp_root: SecretRoot,
    /// The seat's own `steam.exe`, when the box has Steam.
    steam: Option<PathBuf>,
    environment: Vec<u16>,
    workdir: PathBuf,
    /// The gate and the host write here, as the box host writes `logs\host.log`.
    log: PathBuf,
}

/// The running gate and the file under the seat's temporary root it encodes into.
struct GateRun {
    process: ChildProcess,
    output: String,
}

impl Rig {
    fn probe(&self, look: Look) -> WinResult<Probe> {
        let exit = |child: Option<&ChildProcess>| match child {
            Some(child) => child.exit_code(),
            None => Ok(None),
        };
        let keeper = self.keeper.as_ref().map(|(keeper, _)| keeper);
        let host = self.host.as_ref();
        let mut probe = Probe::default();
        match look {
            Look::Clock => {}
            Look::Session => {
                probe.keeper = exit(keeper)?;
                if probe.keeper.is_none() {
                    probe.session =
                        wts::active_for_account(&self.seat.account)?.map(|session| session.id);
                }
            }
            Look::Gate => {
                probe.gate = exit(self.gate.as_ref().map(|gate| &gate.process))?;
                // A keeper that can't be asked reads as running: the gate's own exit decides.
                probe.keeper = exit(keeper).ok().flatten();
            }
            Look::Ports => {
                probe.keeper = exit(keeper)?;
                probe.host = exit(host)?;
                // Native is QUIC: ready once this host holds the UDP port, not when TCP answers.
                probe.ready = probe.keeper.is_none()
                    && probe.host.is_none()
                    && host.is_some_and(|host| {
                        udp_port_owners(self.seat.native_port).contains(&host.pid())
                            && port_open(self.seat.mgmt_port)
                    });
            }
            Look::Exits => {
                probe.keeper = exit(keeper)?;
                probe.host = exit(host)?;
            }
            Look::Keeper => probe.keeper = exit(keeper)?,
        }
        Ok(probe)
    }

    fn perform(&mut self, action: Action) -> WinResult<()> {
        let seat = &self.seat.id;
        match action {
            Action::Announce(stage, attempt) => {
                tracing::info!(%seat, step = stage.name(), attempt, "seat start step");
            }
            Action::StartKeeper => return self.start_keeper(),
            Action::EnterSession(id) => return self.enter_session(id),
            Action::StartGate => return self.start_gate(),
            Action::CollectGate => return self.collect_gate(),
            Action::StartHost => return self.start_host(),
            Action::LogHostExit(code) => {
                tracing::warn!(%seat, code, "seat host exited; restarting");
            }
            Action::EndCycle => self.end_cycle(),
            Action::Report { error, retry } => {
                tracing::warn!(
                    %seat,
                    code = %error.code,
                    retry,
                    "seat did not start: {}",
                    error.message
                );
            }
            Action::Wait(duration) => std::thread::sleep(duration),
            Action::Finish => {}
        }
        Ok(())
    }

    fn start_keeper(&mut self) -> WinResult<()> {
        let pin = rdp::load_pin(&self.root)?;
        let credential = self.accounts.credential_for_start(&self.seat)?;
        let bootstrap =
            RdpBootstrap::new(self.seat.account.clone(), credential.copy_zeroizing(), pin)
                .map_err(|error| {
                    io_error("keeper_bootstrap", "build RDP keeper bootstrap", error)
                })?;
        let job = Job::new()?;
        let keeper_exe = super::keeper::keeper_path(&self.host_path);
        let keeper = process::spawn_keeper(&job, &keeper_exe, &bootstrap)?;
        self.keeper = Some((keeper, job));
        Ok(())
    }

    fn enter_session(&mut self, id: u32) -> WinResult<()> {
        self.session = Some(id);
        let seat = &self.seat;
        // The profile exists once the session does. A first copy runs beside the start; the seat's
        // Steam titles start once it lands, and its launchers are pointed at it then.
        super::steam::ensure_copy(id, &seat.account, seat.id.as_str());
        let steam = super::steam::box_install().and(super::steam::seat_exe(&seat.account));
        let host_root = self
            .root
            .open_child_dir("hosts")
            .and_then(|hosts| hosts.open_child_dir(seat.id.as_str()))
            .map_err(|error| io_error("seat_root", "open per-seat host root", error))?;
        let temp_root = self
            .root
            .open_child_dir("temp")
            .and_then(|temp| temp.open_child_dir(seat.id.as_str()))
            .map_err(|error| io_error("seat_root", "open per-seat temporary root", error))?;
        let environment =
            process::seat_environment(host_root.path(), seat, steam.as_deref(), true)?;
        let workdir = self.host_path.parent().ok_or_else(|| {
            backend_error(
                "host_missing",
                "configured host executable has no parent directory",
            )
        })?;
        let log = host_root
            .child_path("host.log")
            .map_err(|error| io_error("seat_log", "resolve the seat log", error))?;
        self.launch = Some(Launch {
            temp_root,
            steam,
            environment,
            workdir: workdir.to_owned(),
            log,
            host_root,
        });
        Ok(())
    }

    /// The session and what its children start with. `EnterSession` comes before every
    /// action that asks.
    fn launch(&self) -> WinResult<(u32, &Launch)> {
        match (self.session, &self.launch) {
            (Some(session), Some(launch)) => Ok((session, launch)),
            _ => Err(backend_error(
                "seat_session",
                "the seat's session is not prepared",
            )),
        }
    }

    /// Start the gate: one second of the host's own encode from a virtual display in the
    /// seat's session. It extends the display instead of isolating it.
    fn start_gate(&mut self) -> WinResult<()> {
        let (session, launch) = self.launch()?;
        let environment = process::seat_environment(
            launch.host_root.path(),
            &self.seat,
            launch.steam.as_deref(),
            false,
        )?;
        let mut random = [0_u8; 8];
        rand::rng().fill_bytes(&mut random);
        let name = format!("quality-{}-{}.h265", self.seat.id, hex::encode(random));
        launch
            .temp_root
            .write_atomic(&name, &[])
            .map_err(|error| io_error("quality_output", "create quality output", error))?;
        let output = launch
            .temp_root
            .child_path(&name)
            .map_err(|error| io_error("quality_output", "resolve quality output", error))?;
        let arguments = [
            "spike",
            "--source",
            "virtual",
            "--width",
            "1280",
            "--height",
            "720",
            "--fps",
            "60",
            "--seconds",
            "1",
            "--codec",
            "h265",
            "--bitrate",
            "5",
            "--no-loopback",
            "--out",
        ]
        .into_iter()
        .map(OsString::from)
        .chain(std::iter::once(output.as_os_str().to_owned()))
        .collect::<Vec<_>>();
        let spawned = process::spawn_in_session(
            session,
            &self.host_path,
            &arguments,
            &environment,
            &launch.workdir,
            &launch.log,
        );
        match spawned {
            Ok(process) => {
                self.gate = Some(GateRun {
                    process,
                    output: name,
                });
                Ok(())
            }
            Err(error) => {
                let _ = launch.temp_root.remove_file(&name);
                Err(error)
            }
        }
    }

    /// The gate exited clean: it passed if it wrote anything. Its output is removed either
    /// way, and an output that can't be removed fails the gate too.
    fn collect_gate(&mut self) -> WinResult<()> {
        let gate = self.gate.take();
        let (_, launch) = self.launch()?;
        let Some(gate) = gate else {
            return Err(backend_error("quality_output", "the gate did not run"));
        };
        let result = launch
            .temp_root
            .read_current(&gate.output, 16 * 1024 * 1024)
            .map_err(|error| io_error("quality_output", "read quality output", error))
            .and_then(|bytes| {
                if bytes.unwrap_or_default().is_empty() {
                    return Err(backend_error(
                        "quality_empty",
                        "same-session virtual-display quality gate produced no output",
                    ));
                }
                Ok(())
            });
        let cleanup = launch
            .temp_root
            .remove_file(&gate.output)
            .map_err(|error| io_error("quality_output", "remove quality output", error));
        result.and(cleanup)
    }

    fn start_host(&mut self) -> WinResult<()> {
        let (session, launch) = self.launch()?;
        let host = process::spawn_in_session(
            session,
            &self.host_path,
            &[OsString::from("serve")],
            &launch.environment,
            &launch.workdir,
            &launch.log,
        )?;
        self.host = Some(host);
        Ok(())
    }

    /// End the cycle in this order: the host and the gate, the keeper and its job, then
    /// the session. A session child ends when its handle drops.
    fn end_cycle(&mut self) {
        self.host = None;
        if let (Some(gate), Some(launch)) = (self.gate.take(), &self.launch) {
            drop(gate.process);
            let _ = launch.temp_root.remove_file(&gate.output);
        }
        self.launch = None;
        self.keeper = None;
        if let Some(session) = self.session.take() {
            let _ = wts::logoff(session);
        }
    }
}

fn ensure_ports_free(seat: &Seat) -> WinResult<()> {
    let native = seat.native_port;
    if let Some(pid) = udp_port_owners(native).first() {
        return Err(backend_error(
            "port_in_use",
            format!("assigned native port {native} is already held by process {pid}"),
        ));
    }
    let mgmt = seat.mgmt_port;
    if port_open(mgmt) {
        return Err(backend_error(
            "port_in_use",
            format!("assigned management port {mgmt} already accepts connections"),
        ));
    }
    Ok(())
}
