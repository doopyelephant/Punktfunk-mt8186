//! What a failed seat cycle means, read from its error code once.
//!
//! The Windows modules answer with a [`BackendError`], whose code is a string on the
//! wire. [`Fault::from`] is the one place that string is compared: the supervisor's retry
//! rule and the console's sentence match on [`Code`]. Pure, so its tests run on every
//! target.

// Off Windows only the tests read this module.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::backend::BackendError;

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
