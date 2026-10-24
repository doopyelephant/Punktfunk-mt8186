//! Take-turns profiles on Windows: a profile bound to a local Windows account that takes the one
//! console in turn (`design/windows-take-turns.md`). One at a time, so no Remote Desktop, no
//! Server, no concurrency — it is the box's single interactive console, switched between accounts
//! the way Windows' own *Switch user* does.
//!
//! The decision — play here, switch the console to this account, or tell the player to sign in
//! once — is a pure function of what Windows reports, so it is tested on every target. The
//! Windows-only shell ([`crate::windows::console`]) reads the sessions and runs the switch.

/// What Windows reports about a take-turns profile's account, as the switch needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountState {
    /// The account still exists on this PC.
    pub exists: bool,
    /// Its session is the console right now, so it is already on screen.
    pub on_console: bool,
    /// It has a non-console local session (active or disconnected) to attach to the console,
    /// and this is its WTS id. `None` means it has not signed in since boot.
    pub switchable_session: Option<u32>,
}

impl AccountState {
    /// The absent account: every non-Windows host, and a Windows account that is gone.
    pub(crate) fn absent() -> AccountState {
        AccountState {
            exists: false,
            on_console: false,
            switchable_session: None,
        }
    }
}

/// What the console must do before a take-turns profile can play.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConsoleAction {
    /// The console is already this account; the connect plays here.
    Here,
    /// Attach this session to the console, then the service relaunches the host into it.
    Switch(u32),
    /// The account has no session to attach; it must sign in on the PC once (cold sign-in is
    /// not built — `design/windows-take-turns.md` §6).
    SignInFirst,
    /// The bound Windows account is gone.
    Gone,
}

/// The action for an account in this state. Pure: the Windows shell reads [`AccountState`], this
/// decides.
pub(crate) fn decide(state: &AccountState) -> ConsoleAction {
    if !state.exists {
        return ConsoleAction::Gone;
    }
    if state.on_console {
        return ConsoleAction::Here;
    }
    match state.switchable_session {
        Some(id) => ConsoleAction::Switch(id),
        None => ConsoleAction::SignInFirst,
    }
}

/// A take-turns profile's seat as a client reads it. `console_busy` is true when a stream is live
/// on the console as a *different* account: a switch would yank it, so the seat reads `occupied`
/// and the host refuses the switch rather than cut someone's game (the no-visible-glitch bar).
/// `switching` is a switch to this account in flight (the marker, within its timeout).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TurnSeat {
    /// On screen now: dial.
    Ready,
    /// A switch is under way: show the line and wait.
    Switching,
    /// Off screen, ready to switch in: the client's `wake` triggers it.
    Switchable,
    /// Signed in nowhere: tell the player to sign in once.
    SignIn,
    /// Someone else holds the console: don't switch under them.
    Occupied,
    /// The account is gone.
    Gone,
}

/// The seat state for a take-turns profile. `switching` short-circuits a `Switch` so the card
/// reads "Switching to …" through the host relaunch instead of inviting a second wake.
pub(crate) fn turn_seat(action: &ConsoleAction, console_busy: bool, switching: bool) -> TurnSeat {
    match action {
        ConsoleAction::Gone => TurnSeat::Gone,
        ConsoleAction::SignInFirst => TurnSeat::SignIn,
        ConsoleAction::Here => TurnSeat::Ready,
        ConsoleAction::Switch(_) if switching => TurnSeat::Switching,
        ConsoleAction::Switch(_) if console_busy => TurnSeat::Occupied,
        ConsoleAction::Switch(_) => TurnSeat::Switchable,
    }
}

/// How long a switch marker counts as in flight before a fresh `wake` may try again: the console
/// change plus the host relaunch. Past this, the marker is stale (the switch never took) and the
/// card goes back to `Switchable`.
pub(crate) const SWITCH_TIMEOUT_SECS: i64 = 60;

/// Whether a switch marker written at `marked_unix` for `marked_account` is still in flight for a
/// connect asking `asked_account`, at `now_unix`. A marker for another account, or one past
/// [`SWITCH_TIMEOUT_SECS`], is not.
pub(crate) fn switch_in_flight(
    marker: Option<(&str, i64)>,
    asked_account: &str,
    now_unix: i64,
) -> bool {
    match marker {
        Some((account, at)) => {
            account.eq_ignore_ascii_case(asked_account)
                && now_unix.saturating_sub(at) <= SWITCH_TIMEOUT_SECS
        }
        None => false,
    }
}

/// The box's current console account, lowercased by the caller when it compares. `None` off
/// Windows and at boot / the sign-in screen.
#[cfg(windows)]
pub(crate) fn console_account() -> Option<String> {
    super::windows_console_user()
}
#[cfg(not(windows))]
pub(crate) fn console_account() -> Option<String> {
    None
}

/// What Windows reports about `account`'s session right now. Absent everywhere else.
#[cfg(windows)]
pub(crate) fn account_state(account: &str) -> AccountState {
    crate::windows::console::account_state(account)
}
#[cfg(not(windows))]
pub(crate) fn account_state(_account: &str) -> AccountState {
    AccountState::absent()
}

/// Attach `account`'s `session_id` to the console and mark the switch in flight. The service's
/// console-change relaunch then brings the host up in that session. Windows only.
#[cfg(windows)]
pub(crate) fn switch_to(account: &str, session_id: u32) -> anyhow::Result<()> {
    crate::windows::console::switch_to(account, session_id)
}
#[cfg(not(windows))]
pub(crate) fn switch_to(_account: &str, _session_id: u32) -> anyhow::Result<()> {
    anyhow::bail!("take-turns profiles are Windows only")
}

/// The switch marker (account, written-unix) if one is set, else `None`.
#[cfg(windows)]
pub(crate) fn switch_marker() -> Option<(String, i64)> {
    crate::windows::console::read_switch_marker()
}
#[cfg(not(windows))]
pub(crate) fn switch_marker() -> Option<(String, i64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(exists: bool, on_console: bool, session: Option<u32>) -> AccountState {
        AccountState {
            exists,
            on_console,
            switchable_session: session,
        }
    }

    #[test]
    fn a_gone_account_is_gone_whatever_else() {
        assert_eq!(decide(&state(false, true, Some(3))), ConsoleAction::Gone);
        assert_eq!(decide(&AccountState::absent()), ConsoleAction::Gone);
    }

    #[test]
    fn on_the_console_plays_here() {
        assert_eq!(decide(&state(true, true, None)), ConsoleAction::Here);
    }

    #[test]
    fn a_signed_in_account_switches_a_cold_one_signs_in() {
        assert_eq!(
            decide(&state(true, false, Some(4))),
            ConsoleAction::Switch(4)
        );
        assert_eq!(
            decide(&state(true, false, None)),
            ConsoleAction::SignInFirst
        );
    }

    #[test]
    fn a_busy_console_holds_a_switchable_account_off() {
        let switch = ConsoleAction::Switch(4);
        assert_eq!(turn_seat(&switch, false, false), TurnSeat::Switchable);
        assert_eq!(turn_seat(&switch, true, false), TurnSeat::Occupied);
        // A switch in flight shows the wait even while the old console is still busy.
        assert_eq!(turn_seat(&switch, true, true), TurnSeat::Switching);
    }

    #[test]
    fn here_and_signin_ignore_busy_and_switching() {
        assert_eq!(turn_seat(&ConsoleAction::Here, true, true), TurnSeat::Ready);
        assert_eq!(
            turn_seat(&ConsoleAction::SignInFirst, true, true),
            TurnSeat::SignIn
        );
    }

    #[test]
    fn a_switch_marker_is_in_flight_only_for_its_account_within_the_timeout() {
        assert!(switch_in_flight(Some(("alex", 100)), "ALEX", 130));
        assert!(switch_in_flight(Some(("alex", 100)), "alex", 160));
        assert!(!switch_in_flight(
            Some(("alex", 100)),
            "alex",
            100 + SWITCH_TIMEOUT_SECS + 1
        ));
        assert!(!switch_in_flight(Some(("joe", 100)), "alex", 110));
        assert!(!switch_in_flight(None, "alex", 110));
    }
}
