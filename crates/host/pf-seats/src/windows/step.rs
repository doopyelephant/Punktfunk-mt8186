//! One Windows seat's lifecycle as a pure step function.
//!
//! [`State`] is everything a seat's worker and its callers know. [`State::step`] takes one
//! [`Observation`] and the time, moves the state and answers the [`Action`]s the worker
//! performs. No I/O and no OS types, so the tests run on every target; `supervisor.rs`
//! probes what [`State::look`] names and performs.
//!
//! A cycle is one keeper and its session: sign in, pass the display gate, start the host,
//! watch both. The gate is owed once per seat start. A host that exits starts again in the
//! same session. A fault ends the cycle, and the next one signs in again unless the fault is
//! permanent ([`Code::permanent`]) or a stop was asked.
//!
//! A [`BackendError`]'s code is a string on the wire. [`Fault::from`] is the one place it is
//! compared: the retry rule and the console's sentence match on [`Code`].

// Off Windows only the tests read this module.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::backend::BackendError;
use crate::model::{RuntimeState, RuntimeStatus};
use std::time::{Duration, Instant};

const SESSION_TIMEOUT: Duration = Duration::from_secs(45);
const QUALITY_TIMEOUT: Duration = Duration::from_secs(60);
/// Fresh sessions a failing gate gets. A session whose display misses its path never
/// gets one; a fresh logon passes about 40% of the time on 9300.
const QUALITY_SESSIONS: u32 = 5;
const HOST_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Room for a slow first logon plus two gate sessions, the usual worst healthy start.
pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(180);
const FIRST_RESTART_DELAY: Duration = Duration::from_secs(1);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);

/// How long the worker sleeps between two looks of one kind.
const SESSION_POLL: Duration = Duration::from_millis(250);
const GATE_POLL: Duration = Duration::from_millis(100);
const READY_POLL: Duration = Duration::from_millis(200);
const WATCH_POLL: Duration = Duration::from_millis(250);
const DELAY_POLL: Duration = Duration::from_millis(100);

/// A cycle error's code, for every code the supervisor acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Code {
    WtsTimeout,
    KeeperExited,
    QualityFailed,
    /// Any other `quality_` code: the gate reached no verdict.
    Quality,
    HostReadyTimeout,
    HostExited,
    PortInUse,
    SeatsOff,
    TermserviceStopped,
    SeatDisplayDriver,
    AccountNotOwned,
    AccountIsAdministrator,
    AccountPolicy,
    CredentialMissing,
    CredentialInvalid,
    CredentialVerify,
    HostMissing,
    RdpPinMissing,
    RdpPinInvalid,
    LocalSystemRequired,
    Other,
}

impl Code {
    fn of(code: &str) -> Self {
        match code {
            "wts_timeout" => Self::WtsTimeout,
            "rdp_keeper_exited" => Self::KeeperExited,
            "quality_failed" => Self::QualityFailed,
            "host_ready_timeout" => Self::HostReadyTimeout,
            "host_exited" => Self::HostExited,
            "port_in_use" => Self::PortInUse,
            "seats_off" => Self::SeatsOff,
            "termservice_stopped" => Self::TermserviceStopped,
            "seat_display_driver" => Self::SeatDisplayDriver,
            "account_not_owned" => Self::AccountNotOwned,
            "account_is_administrator" => Self::AccountIsAdministrator,
            "account_policy" => Self::AccountPolicy,
            "credential_missing" => Self::CredentialMissing,
            "credential_invalid" => Self::CredentialInvalid,
            "credential_verify" => Self::CredentialVerify,
            "host_missing" => Self::HostMissing,
            "rdp_pin_missing" => Self::RdpPinMissing,
            "rdp_pin_invalid" => Self::RdpPinInvalid,
            "local_system_required" => Self::LocalSystemRequired,
            _ if code.starts_with("quality_") => Self::Quality,
            _ => Self::Other,
        }
    }

    /// Whether a new cycle can't help: the gate gave its answer, or the operator has to fix
    /// the account, its credential, the host binary or the pin first.
    pub(crate) fn permanent(self) -> bool {
        match self {
            Self::QualityFailed
            | Self::Quality
            | Self::AccountNotOwned
            | Self::AccountIsAdministrator
            | Self::AccountPolicy
            | Self::CredentialMissing
            | Self::CredentialInvalid
            | Self::CredentialVerify
            | Self::HostMissing
            | Self::RdpPinMissing
            | Self::RdpPinInvalid
            | Self::LocalSystemRequired => true,
            Self::WtsTimeout
            | Self::KeeperExited
            | Self::HostReadyTimeout
            | Self::HostExited
            | Self::PortInUse
            | Self::SeatsOff
            | Self::TermserviceStopped
            | Self::SeatDisplayDriver
            | Self::Other => false,
        }
    }
}

/// A cycle error with its code read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fault {
    pub(crate) code: Code,
    pub(crate) error: BackendError,
}

impl From<BackendError> for Fault {
    fn from(error: BackendError) -> Self {
        Self {
            code: Code::of(&error.code),
            error,
        }
    }
}

impl Fault {
    /// The sentence the console shows for a seat that didn't start, on its card and in
    /// Doctor. The log keeps the code and the operator's message.
    pub(crate) fn plain(&self) -> String {
        let text = match self.code {
            Code::WtsTimeout | Code::KeeperExited => {
                "Windows didn't sign in to the seat's account. Remote Desktop may be off or out of licences."
            }
            Code::QualityFailed | Code::Quality => {
                "The seat's screen didn't start. The graphics card or the seat display driver failed its check."
            }
            Code::HostReadyTimeout | Code::HostExited => {
                "punktfunk didn't start on the seat. The seat's log has the cause."
            }
            Code::PortInUse => {
                "Another program holds the seat's port. Restart the PC, then try again."
            }
            Code::SeatsOff => "Seats are off. Turn them on under Profiles → Seats.",
            Code::TermserviceStopped => "Remote Desktop Services isn't running. Restart the PC.",
            Code::SeatDisplayDriver => {
                "The seat display driver isn't installed. Turn seats off and on again under Profiles → Seats."
            }
            Code::AccountNotOwned
            | Code::AccountIsAdministrator
            | Code::AccountPolicy
            | Code::CredentialMissing
            | Code::CredentialInvalid
            | Code::CredentialVerify
            | Code::HostMissing
            | Code::RdpPinMissing
            | Code::RdpPinInvalid
            | Code::LocalSystemRequired
            | Code::Other => return self.error.message.clone(),
        };
        text.into()
    }
}

/// The stages of a seat start, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    SignIn,
    Display,
    Host,
}

impl Stage {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::SignIn => "sign_in",
            Self::Display => "display_check",
            Self::Host => "host_ready",
        }
    }

    fn phrase(self) -> &'static str {
        match self {
            Self::SignIn => "signing in to the seat's Windows account",
            Self::Display => "checking the seat's screen and graphics card",
            Self::Host => "starting punktfunk on the seat",
        }
    }

    /// What the console shows while the seat is on this stage. `attempt` counts gate sessions.
    fn detail(self, attempt: u32) -> String {
        let phrase = self.phrase();
        let mut text = phrase[..1].to_uppercase() + &phrase[1..];
        if attempt > 1 {
            text += &format!(" (try {attempt} of {QUALITY_SESSIONS})");
        }
        text + "…"
    }

    /// The console's sentence for a start that ran out of time on this stage.
    fn timed_out(self) -> String {
        format!(
            "The seat didn't start in time. It stopped while {}.",
            self.phrase()
        )
    }
}

/// What the worker probes for the next step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Look {
    /// Nothing: only the time has moved.
    Clock,
    /// The keeper, and the account's session while the keeper lives.
    Session,
    /// The gate and the keeper.
    Gate,
    /// The keeper, the host, and whether the host holds its two ports.
    Ports,
    /// The keeper and the host.
    Exits,
    /// The keeper alone.
    Keeper,
}

/// What the worker saw. An exit code is `None` while its process runs or was not looked at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Probe {
    pub(crate) keeper: Option<u32>,
    pub(crate) gate: Option<u32>,
    pub(crate) host: Option<u32>,
    /// The id of the account's active session.
    pub(crate) session: Option<u32>,
    /// The host holds its native port and answers on its management port.
    pub(crate) ready: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Observation {
    /// What the worker saw of what [`State::look`] named.
    Probe(Probe),
    /// A probe or an action failed.
    Failed(Fault),
    /// A caller asked the seat to stop.
    Stop,
    /// `start` waited [`START_TIMEOUT`] without an answer.
    StartTimedOut,
}

/// What the worker does next, in order. An action that fails comes back as
/// [`Observation::Failed`] and the actions after it are dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Log that the start entered this stage, on this gate session.
    Announce(Stage, u32),
    /// Start a keeper in a job of its own.
    StartKeeper,
    /// Record this session and build what its children start with.
    EnterSession(u32),
    StartGate,
    /// Read the gate's output and remove it: empty output fails the gate.
    CollectGate,
    /// Start the host, in place of one that exited.
    StartHost,
    /// Log that the host exited with this code and starts again.
    LogHostExit(u32),
    /// End the host and the gate, then the keeper and its job, then log the session off.
    EndCycle,
    /// Log a cycle that failed, and whether another follows.
    Report {
        error: BackendError,
        retry: bool,
    },
    /// Sleep, then look again.
    Wait(Duration),
    /// Leave the worker loop.
    Finish,
}

/// Why the seat is stopping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Asked,
    /// The start ran out of time on this stage: the seat then reads failed, not stopped.
    TimedOut(Stage),
}

/// The display gate, owed once per seat start. `failures` counts the sessions it failed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    Owed { failures: u32 },
    Passed,
}

/// When a wait began: at its first look, which follows the action that started the child.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Since(Option<Instant>);

impl Since {
    fn over(&mut self, now: Instant, limit: Duration) -> bool {
        now >= *self.0.get_or_insert(now) + limit
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    /// No keeper. The next cycle starts once `until` passes; `None` starts it at once.
    Resting { until: Option<Instant> },
    /// The keeper is up and the account's session is awaited.
    SigningIn { since: Since },
    /// The session is active and its children's roots and environment are being built.
    Entering,
    /// The gate runs in the session.
    Gating { since: Since },
    /// The gate exited clean and its output is being read.
    Collecting,
    /// The host runs and its ports are awaited.
    Readying { since: Since },
    /// The host serves. The keeper and the host are watched.
    Running,
    /// The host exited and starts again at `until`.
    HostDown { until: Instant },
    /// The cycle is being torn down, for this fault or for a stop.
    Ending { fault: Option<Fault> },
    /// The worker has left.
    Finished,
}

/// Everything known about one seat's runtime. Only [`State::step`] changes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct State {
    account: String,
    phase: Phase,
    /// The start stage last entered, where a start that runs out of time stood.
    stage: Stage,
    status: RuntimeStatus,
    session: Option<u32>,
    /// The first start's outcome, which `start` waits for.
    answer: Option<Result<(), BackendError>>,
    stop: Option<Stop>,
    gate: Gate,
    /// The wait before the next cycle. It doubles and never shrinks.
    cycle_delay: Duration,
    /// The wait before the host starts again. It doubles within a cycle.
    host_delay: Duration,
}

impl State {
    pub(crate) fn new(account: String) -> Self {
        Self {
            account,
            phase: Phase::Resting { until: None },
            stage: Stage::SignIn,
            status: starting(Stage::SignIn.detail(0)),
            session: None,
            answer: None,
            stop: None,
            gate: Gate::Owed { failures: 0 },
            cycle_delay: FIRST_RESTART_DELAY,
            host_delay: FIRST_RESTART_DELAY,
        }
    }

    pub(crate) fn account(&self) -> &str {
        &self.account
    }

    pub(crate) fn status(&self) -> &RuntimeStatus {
        &self.status
    }

    /// The seat's session, from the moment it is active until its cycle has ended.
    pub(crate) fn session(&self) -> Option<u32> {
        self.session
    }

    /// The first start's outcome. It stays unset through a failed gate that gets another
    /// session, and when the start runs out of time.
    pub(crate) fn answer(&self) -> Option<&Result<(), BackendError>> {
        self.answer.as_ref()
    }

    pub(crate) fn finished(&self) -> bool {
        self.phase == Phase::Finished
    }

    /// What `start` answers when it has waited [`START_TIMEOUT`].
    pub(crate) fn start_timeout(&self) -> BackendError {
        BackendError::new(
            "start_timeout",
            format!(
                "seat did not become ready within {} seconds while {}",
                START_TIMEOUT.as_secs(),
                self.stage.phrase()
            ),
        )
    }

    pub(crate) fn look(&self) -> Look {
        match self.phase {
            Phase::SigningIn { .. } => Look::Session,
            Phase::Gating { .. } => Look::Gate,
            Phase::Readying { .. } => Look::Ports,
            Phase::Running => Look::Exits,
            Phase::HostDown { .. } => Look::Keeper,
            Phase::Resting { .. }
            | Phase::Entering
            | Phase::Collecting
            | Phase::Ending { .. }
            | Phase::Finished => Look::Clock,
        }
    }

    /// Take one observation at `now`. A caller's request only marks the state: the worker
    /// acts on it at its next look, so those two answer no actions.
    pub(crate) fn step(&mut self, observation: Observation, now: Instant) -> Vec<Action> {
        match observation {
            Observation::Stop => {
                self.stop.get_or_insert(Stop::Asked);
                Vec::new()
            }
            Observation::StartTimedOut => {
                self.stop = Some(Stop::TimedOut(self.stage));
                self.status = RuntimeStatus::failed(self.stage.timed_out());
                Vec::new()
            }
            Observation::Failed(_) if self.finished() => vec![Action::Finish],
            Observation::Failed(fault) => self.end(Some(fault)),
            Observation::Probe(probe) => self.probed(probe, now),
        }
    }

    /// One look, in the phase's own order: which of a stop, an exit and a limit wins there
    /// decides the code the caller reads.
    fn probed(&mut self, probe: Probe, now: Instant) -> Vec<Action> {
        match self.phase {
            Phase::Resting { until } => self.resting(until, now),
            Phase::SigningIn { ref mut since } => {
                let late = since.over(now, SESSION_TIMEOUT);
                self.signing_in(probe, late)
            }
            Phase::Entering => self.entered(),
            Phase::Gating { ref mut since } => {
                let late = since.over(now, QUALITY_TIMEOUT);
                self.gating(probe, late)
            }
            Phase::Collecting => {
                self.gate = Gate::Passed;
                self.start_host()
            }
            Phase::Readying { ref mut since } => {
                let late = since.over(now, HOST_READY_TIMEOUT);
                self.readying(probe, late)
            }
            Phase::Running => self.running(probe, now),
            Phase::HostDown { until } => self.host_down(probe, until, now),
            Phase::Ending { ref mut fault } => {
                let fault = fault.take();
                self.ended(fault, now)
            }
            Phase::Finished => vec![Action::Finish],
        }
    }

    fn stopping(&self) -> bool {
        self.stop.is_some()
    }

    /// A stop, then the rest. A cycle starts once the rest is over.
    fn resting(&mut self, until: Option<Instant>, now: Instant) -> Vec<Action> {
        if self.stopping() {
            self.finish()
        } else if until.is_some_and(|until| now < until) {
            vec![Action::Wait(DELAY_POLL)]
        } else {
            self.host_delay = FIRST_RESTART_DELAY;
            self.phase = Phase::SigningIn {
                since: Since::default(),
            };
            self.enter(Stage::SignIn, 0, Action::StartKeeper)
        }
    }

    /// A stop or a lost keeper, then the session, then the limit. A stop reads as a lost
    /// keeper here.
    fn signing_in(&mut self, probe: Probe, late: bool) -> Vec<Action> {
        if self.stopping() || probe.keeper.is_some() {
            self.fail(
                "rdp_keeper_exited",
                "RDP keeper exited before its WTS session became active",
            )
        } else if let Some(id) = probe.session {
            self.session = Some(id);
            self.phase = Phase::Entering;
            vec![Action::EnterSession(id)]
        } else if late {
            let account = &self.account;
            let message = format!("no active non-console WTS session appeared for '{account}'");
            self.fail("wts_timeout", message)
        } else {
            vec![Action::Wait(SESSION_POLL)]
        }
    }

    /// The session is prepared: the gate while it is owed, else the host. No stop is read.
    fn entered(&mut self) -> Vec<Action> {
        match self.gate {
            Gate::Owed { failures } => {
                self.phase = Phase::Gating {
                    since: Since::default(),
                };
                self.enter(Stage::Display, failures + 1, Action::StartGate)
            }
            Gate::Passed => self.start_host(),
        }
    }

    /// The gate's exit first. A stop, a lost keeper and the limit all read as a gate out of
    /// time, which no new cycle follows.
    fn gating(&mut self, probe: Probe, late: bool) -> Vec<Action> {
        match probe.gate {
            Some(0) => {
                self.phase = Phase::Collecting;
                vec![Action::CollectGate]
            }
            Some(code) => self.fail(
                "quality_failed",
                format!("same-session virtual-display quality gate exited with code {code}"),
            ),
            None if self.stopping() || probe.keeper.is_some() || late => self.fail(
                "quality_timeout",
                format!(
                    "same-session virtual-display quality gate did not finish within {} seconds",
                    QUALITY_TIMEOUT.as_secs()
                ),
            ),
            None => vec![Action::Wait(GATE_POLL)],
        }
    }

    /// A stop, a lost keeper, a host that exited, the ports, then the limit.
    fn readying(&mut self, probe: Probe, late: bool) -> Vec<Action> {
        if self.stopping() {
            self.fail(
                "seat_stopping",
                "seat stopped while waiting for host readiness",
            )
        } else if let Some(code) = probe.keeper {
            self.fail(
                "rdp_keeper_exited",
                format!("RDP keeper exited with code {code} during host startup"),
            )
        } else if let Some(code) = probe.host {
            self.fail(
                "host_exited",
                format!("punktfunk-host exited with code {code} before readiness"),
            )
        } else if probe.ready {
            self.status = RuntimeStatus::running();
            self.answer.get_or_insert(Ok(()));
            self.phase = Phase::Running;
            Vec::new()
        } else if late {
            self.fail(
                "host_ready_timeout",
                format!(
                    "punktfunk-host did not open both assigned ports within {} seconds",
                    HOST_READY_TIMEOUT.as_secs()
                ),
            )
        } else {
            vec![Action::Wait(READY_POLL)]
        }
    }

    /// A stop, a lost keeper, then a host that exited, which starts again after a wait.
    fn running(&mut self, probe: Probe, now: Instant) -> Vec<Action> {
        if self.stopping() {
            self.status = RuntimeStatus {
                state: RuntimeState::Stopping,
                detail: None,
            };
            self.end(None)
        } else if let Some(code) = probe.keeper {
            self.fail(
                "rdp_keeper_exited",
                format!("RDP keeper process exited with code {code}"),
            )
        } else if let Some(code) = probe.host {
            let delay = self.host_delay;
            self.host_delay = doubled(delay);
            self.status = starting(format!(
                "punktfunk stopped on the seat. Restarting it in {} s…",
                delay.as_secs()
            ));
            self.phase = Phase::HostDown { until: now + delay };
            vec![Action::LogHostExit(code)]
        } else {
            vec![Action::Wait(WATCH_POLL)]
        }
    }

    /// The wait's end first: a host that is due starts even with a stop asked, and its ready
    /// wait reads the stop. Then a stop, then a lost keeper.
    fn host_down(&mut self, probe: Probe, until: Instant, now: Instant) -> Vec<Action> {
        if now >= until {
            self.phase = Phase::Readying {
                since: Since::default(),
            };
            vec![Action::StartHost]
        } else if self.stopping() {
            self.end(None)
        } else if probe.keeper.is_some() {
            self.fail(
                "rdp_keeper_exited",
                "RDP keeper exited during host restart backoff",
            )
        } else {
            vec![Action::Wait(DELAY_POLL)]
        }
    }

    /// Enter `stage` of a start: the console shows its detail and the log names it.
    fn enter(&mut self, stage: Stage, attempt: u32, action: Action) -> Vec<Action> {
        self.stage = stage;
        self.status = starting(stage.detail(attempt));
        vec![Action::Announce(stage, attempt), action]
    }

    fn start_host(&mut self) -> Vec<Action> {
        self.phase = Phase::Readying {
            since: Since::default(),
        };
        self.enter(Stage::Host, 0, Action::StartHost)
    }

    fn fail(&mut self, code: &str, message: impl Into<String>) -> Vec<Action> {
        self.end(Some(BackendError::new(code, message).into()))
    }

    /// Tear the cycle down. What the fault means is decided once that is done, so the
    /// console reads the old status until the session is logged off.
    fn end(&mut self, fault: Option<Fault>) -> Vec<Action> {
        self.phase = Phase::Ending { fault };
        vec![Action::EndCycle]
    }

    /// The cycle is torn down: rest before another one, or finish.
    fn ended(&mut self, fault: Option<Fault>, now: Instant) -> Vec<Action> {
        self.session = None;
        let Some(fault) = fault else {
            return self.finish();
        };
        let stopping = self.stopping();
        // A failed gate gets a fresh session, and the start waits for those.
        let mut gate_retry = false;
        if fault.code == Code::QualityFailed
            && let Gate::Owed { failures } = &mut self.gate
        {
            *failures += 1;
            gate_retry = *failures < QUALITY_SESSIONS;
        }
        let retry = !stopping && (gate_retry || !fault.code.permanent());
        let mut actions = Vec::new();
        // The first answer reaches the caller; later failures have only the log.
        if !stopping && (self.answer.is_some() || gate_retry) {
            actions.push(Action::Report {
                error: fault.error.clone(),
                retry,
            });
        }
        if !gate_retry {
            self.status = RuntimeStatus::failed(fault.plain());
            self.answer.get_or_insert(Err(fault.error));
        }
        if retry {
            self.phase = Phase::Resting {
                until: Some(now + self.cycle_delay),
            };
            self.cycle_delay = doubled(self.cycle_delay);
        } else {
            actions.extend(self.finish());
        }
        actions
    }

    /// The worker leaves. A stopped seat reads stopped, or failed when its start ran out of
    /// time; a seat that failed for good keeps the sentence of its fault.
    fn finish(&mut self) -> Vec<Action> {
        if let Some(stop) = self.stop {
            self.status = match stop {
                Stop::Asked => RuntimeStatus::stopped(),
                Stop::TimedOut(stage) => RuntimeStatus::failed(stage.timed_out()),
            };
            self.answer.get_or_insert_with(|| {
                Err(BackendError::new(
                    "seat_stopping",
                    "seat stopped before startup completed",
                ))
            });
        }
        self.session = None;
        self.phase = Phase::Finished;
        vec![Action::Finish]
    }
}

fn starting(detail: String) -> RuntimeStatus {
    RuntimeStatus {
        state: RuntimeState::Starting,
        detail: Some(detail),
    }
}

fn doubled(delay: Duration) -> Duration {
    delay.saturating_mul(2).min(MAX_RESTART_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGN_IN: &str = "Windows didn't sign in to the seat's account.";
    const SCREEN: &str = "The seat's screen didn't start.";
    const HOST: &str = "punktfunk didn't start on the seat.";
    const MESSAGE: &str = "the operator's message";

    /// Code, whether a retry is pointless, and how the console's sentence starts.
    const CODES: [(&str, bool, &str); 27] = [
        ("wts_timeout", false, SIGN_IN),
        ("rdp_keeper_exited", false, SIGN_IN),
        ("quality_failed", true, SCREEN),
        ("quality_timeout", true, SCREEN),
        ("quality_empty", true, SCREEN),
        ("quality_output", true, SCREEN),
        ("quality_anything_else", true, SCREEN),
        ("host_ready_timeout", false, HOST),
        ("host_exited", false, HOST),
        (
            "port_in_use",
            false,
            "Another program holds the seat's port.",
        ),
        ("seats_off", false, "Seats are off."),
        (
            "termservice_stopped",
            false,
            "Remote Desktop Services isn't running.",
        ),
        (
            "seat_display_driver",
            false,
            "The seat display driver isn't installed.",
        ),
        ("account_not_owned", true, MESSAGE),
        ("account_is_administrator", true, MESSAGE),
        ("account_policy", true, MESSAGE),
        ("credential_missing", true, MESSAGE),
        ("credential_invalid", true, MESSAGE),
        ("credential_verify", true, MESSAGE),
        ("host_missing", true, MESSAGE),
        ("rdp_pin_missing", true, MESSAGE),
        ("rdp_pin_invalid", true, MESSAGE),
        ("local_system_required", true, MESSAGE),
        ("seat_stopping", false, MESSAGE),
        ("process_wait", false, MESSAGE),
        ("wts_logoff", false, MESSAGE),
        ("", false, MESSAGE),
    ];

    #[test]
    fn a_code_keeps_its_retry_rule_and_its_sentence() {
        for (code, permanent, sentence) in CODES {
            let fault = Fault::from(BackendError::new(code, MESSAGE));
            assert_eq!(fault.code.permanent(), permanent, "{code}");
            assert!(fault.plain().starts_with(sentence), "{code}");
            assert_eq!(fault.error.code, code);
        }
    }

    #[test]
    fn only_a_failed_gate_counts_toward_the_gate_sessions() {
        for (code, ..) in CODES {
            let counted = Fault::from(BackendError::new(code, MESSAGE)).code == Code::QualityFailed;
            assert_eq!(counted, code == "quality_failed", "{code}");
        }
    }
}
