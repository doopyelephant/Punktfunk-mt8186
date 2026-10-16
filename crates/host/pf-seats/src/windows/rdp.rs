//! The trusted RDP leaf pin, stored in the seat root.
//!
//! `punktfunk-seat-keeper trust` records the SHA-256 of TermService's TLS leaf here. The
//! supervisor reads it into every keeper's bootstrap, and the keeper refuses any other leaf
//! before CredSSP can send a credential. A changed leaf is never stored without `replace`:
//! turning seats on is the operator's yes, and the supervisor refuses to start a keeper
//! against a leaf that moved since.

use crate::persistence::SecretRoot;
use crate::windows::util::{backend_error, io_error, WinResult};
use std::path::Path;

const PIN_FILE: &str = "rdp-cert-sha256";

/// The console's sentence for a leaf that is no longer the recorded one.
const PIN_CHANGED: &str = "Remote Desktop's certificate has changed, so seats can't sign in. \
    Turn seats off and on under Profiles → Seats to record the new one.";

/// `rdp_pin_changed` when TermService presents a leaf other than `pin`. A leaf that can't be
/// read passes: the keeper's own sign-in says why.
pub(super) fn require_live(host_path: &Path, pin: [u8; 32]) -> WinResult<()> {
    match super::keeper::live_pin(host_path) {
        Some(live) if live != pin => Err(backend_error("rdp_pin_changed", PIN_CHANGED)),
        _ => Ok(()),
    }
}

/// A pin as the keeper prints it: 64 hexadecimal digits.
pub(super) fn parse_pin(text: &str) -> Option<[u8; 32]> {
    hex::decode(text.trim()).ok()?.try_into().ok()
}

/// Record `pin`, refusing to replace a different stored pin unless `replace` is set.
pub fn store_pin(root: &SecretRoot, pin: [u8; 32], replace: bool) -> WinResult<()> {
    if let Some(existing) = load_pin_optional(root)?
        && existing != pin
        && !replace
    {
        return Err(backend_error(
            "rdp_pin_changed",
            format!(
                "RDP leaf changed from {} to {}; rerun with --replace only after verification",
                hex::encode(existing),
                hex::encode(pin)
            ),
        ));
    }
    root.write_atomic(PIN_FILE, format!("{}\n", hex::encode(pin)).as_bytes())
        .map_err(|error| io_error("rdp_pin_store", "write RDP certificate pin", error))
}

pub(super) fn load_pin(root: &SecretRoot) -> WinResult<[u8; 32]> {
    load_pin_optional(root)?.ok_or_else(|| {
        backend_error(
            "rdp_pin_missing",
            "RDP certificate pin is missing; run 'punktfunk-seat-keeper trust' elevated",
        )
    })
}

pub(super) fn load_pin_optional(root: &SecretRoot) -> WinResult<Option<[u8; 32]>> {
    let Some(bytes) = root
        .read_current(PIN_FILE, 128)
        .map_err(|error| io_error("rdp_pin_store", "read RDP certificate pin", error))?
    else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| backend_error("rdp_pin_invalid", "RDP certificate pin is not UTF-8"))?
        .trim();
    if text.len() != 64
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(backend_error(
            "rdp_pin_invalid",
            "RDP certificate pin must be 64 lowercase hexadecimal characters",
        ));
    }
    let decoded = hex::decode(text)
        .map_err(|_| backend_error("rdp_pin_invalid", "RDP certificate pin is invalid"))?;
    let mut pin = [0_u8; 32];
    pin.copy_from_slice(&decoded);
    Ok(Some(pin))
}

#[cfg(test)]
mod tests {
    use super::parse_pin;

    #[test]
    fn the_keeper_prints_a_pin_as_64_hex_digits() {
        let printed = hex::encode([7_u8; 32]);
        assert_eq!(parse_pin(&format!("{printed}\r\n")), Some([7; 32]));
        assert_eq!(parse_pin(&printed[2..]), None);
        assert_eq!(parse_pin("not a pin"), None);
        assert_eq!(parse_pin(""), None);
    }
}
