//! WTS discovery and exact managed-session teardown.
//!
//! Enumeration copies session IDs and states before releasing WTS memory, then
//! queries each user and domain through `WTSQuerySessionInformationW`; a session that ends
//! between the two steps is left out. A usable seat session must be active, non-console,
//! local to this computer, and match the complete account name under Windows case rules.
//! Multiple matches are an error; how long to wait for one is the supervisor's. Disconnected
//! sessions remain visible to diagnostics but are never returned as display-capable.
//! Logoff always takes the exact session ID recorded by the supervisor.

use crate::windows::util::{backend_error, computer_name, io_error, WinResult};
use windows::core::PWSTR;
use windows::Win32::System::RemoteDesktop::{
    WTSActive, WTSDomainName, WTSEnumerateSessionsW, WTSFreeMemory, WTSGetActiveConsoleSessionId,
    WTSLogoffSession, WTSQuerySessionInformationW, WTSUserName, WTS_CONNECTSTATE_CLASS,
    WTS_SESSION_INFOW,
};

#[derive(Clone, Debug)]
pub(super) struct Session {
    pub id: u32,
    pub user: String,
    pub domain: String,
    pub state: WTS_CONNECTSTATE_CLASS,
    pub console: bool,
}

pub(super) fn enumerate() -> WinResult<Vec<Session>> {
    let mut buffer: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0_u32;
    // SAFETY: null server selects the local machine; both output pointers remain live.
    unsafe { WTSEnumerateSessionsW(None, 0, 1, &mut buffer, &mut count) }
        .map_err(|error| io_error("wts_enumerate", "WTSEnumerateSessionsW failed", error))?;
    if buffer.is_null() && count != 0 {
        return Err(backend_error(
            "wts_enumerate",
            "WTSEnumerateSessionsW returned a null buffer",
        ));
    }
    let raw: Vec<(u32, WTS_CONNECTSTATE_CLASS)> = if count == 0 {
        Vec::new()
    } else {
        // SAFETY: WTS returned `count` consecutive session records in the live allocation.
        unsafe { std::slice::from_raw_parts(buffer, count as usize) }
            .iter()
            .map(|session| (session.SessionId, session.State))
            .collect()
    };
    if !buffer.is_null() {
        // SAFETY: this is the allocation returned by WTSEnumerateSessionsW and is freed once.
        unsafe { WTSFreeMemory(buffer.cast()) };
    }

    // SAFETY: this API takes no pointers and returns the current console session ID by value.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    // A session that logged off since the listing has nothing to query: it is gone.
    Ok(raw
        .into_iter()
        .filter_map(|(id, state)| {
            let named = query_string(id, WTSUserName)
                .and_then(|user| Ok((user, query_string(id, WTSDomainName)?)));
            match named {
                Ok((user, domain)) => Some(Session {
                    id,
                    user,
                    domain,
                    state,
                    console: id == console,
                }),
                Err(error) => {
                    tracing::debug!(%error, session = id, "session ended during enumeration");
                    None
                }
            }
        })
        .collect())
}

pub(super) fn active_for_account(account: &str) -> WinResult<Option<Session>> {
    one_account_session(account, true)
}

/// The account's session whatever its state. A logoff removes it; a dropped connection leaves it
/// `Disconnected`, which is the difference between a seat that is gone and one that can be
/// reattached.
pub(super) fn any_for_account(account: &str) -> WinResult<Option<Session>> {
    one_account_session(account, false)
}

pub(super) fn logoff_account_if_present(account: &str) -> WinResult<()> {
    if let Some(session) = one_account_session(account, false)? {
        logoff(session.id)?;
    }
    Ok(())
}

fn one_account_session(account: &str, active_only: bool) -> WinResult<Option<Session>> {
    let machine = computer_name()?;
    let matches: Vec<_> = enumerate()?
        .into_iter()
        .filter(|session| {
            (!active_only || session.state == WTSActive)
                && !session.console
                && session.user.eq_ignore_ascii_case(account)
                && (session.domain.is_empty() || session.domain.eq_ignore_ascii_case(&machine))
        })
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        count => Err(backend_error(
            "wts_ambiguous",
            format!("{count} non-console local sessions match account '{account}'"),
        )),
    }
}

pub(super) fn logoff(session_id: u32) -> WinResult<()> {
    // SAFETY: the caller supplies the exact recorded local WTS session ID; no memory is shared.
    unsafe { WTSLogoffSession(None, session_id, true) }
        .map_err(|error| io_error("wts_logoff", "WTSLogoffSession failed", error))
}

fn query_string(
    session_id: u32,
    class: windows::Win32::System::RemoteDesktop::WTS_INFO_CLASS,
) -> WinResult<String> {
    let mut buffer = PWSTR::null();
    let mut bytes = 0_u32;
    // SAFETY: both outputs remain live and WTS owns any returned allocation until the free below.
    unsafe { WTSQuerySessionInformationW(None, session_id, class, &mut buffer, &mut bytes) }
        .map_err(|error| io_error("wts_query", "WTSQuerySessionInformationW failed", error))?;
    if buffer.is_null() {
        return Ok(String::new());
    }
    let units = bytes as usize / 2;
    // SAFETY: WTS reports `bytes` readable bytes at `buffer`; the allocation is still live.
    let raw = unsafe { std::slice::from_raw_parts(buffer.0, units) };
    let end = raw.iter().position(|unit| *unit == 0).unwrap_or(raw.len());
    let value = String::from_utf16(&raw[..end])
        .map_err(|error| io_error("wts_query", "WTS string is not valid UTF-16", error));
    // SAFETY: this is the allocation returned by WTSQuerySessionInformationW and is freed once.
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    value
}
