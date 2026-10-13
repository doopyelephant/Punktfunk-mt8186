//! What this host reads of the seat contract, in one place.
//!
//! The seat supervisor (`pf-seats`, inside the Windows service) runs one ordinary
//! host per Windows session and marks each with `PUNKTFUNK_SEAT_SESSION=1` plus a
//! `PUNKTFUNK_SEAT_ID`. The host never learns how those sessions come to exist.
//! An unset marker is the console host, which behaves exactly as before.
//!
//! The id is untrusted process input, so it is validated once here rather than
//! at each use: it reaches a device-parameter marker and log lines.
//! `docs-site/content/docs/developers/multi-seat-contract.md` is the contract of record.

/// Whether a supervisor-managed seat owns this host rather than the console.
pub fn is_seat_host() -> bool {
    std::env::var("PUNKTFUNK_SEAT_SESSION").as_deref() == Ok("1")
}

/// 32 lowercase hexadecimal characters, so the id is safe as a device-parameter
/// marker and in a log line. Any other value is a supervisor bug, not a seat.
pub fn validate_seat_id(raw: &str) -> Result<&str, &'static str> {
    let valid = raw.len() == SEAT_ID_LEN
        && raw
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    valid
        .then_some(raw)
        .ok_or("PUNKTFUNK_SEAT_ID must be 32 lowercase hexadecimal characters")
}

const SEAT_ID_LEN: usize = 32;

/// This process's validated seat id, or `None` for the console host.
/// The error is a rejected id, which callers surface rather than ignore.
pub fn seat_id() -> Result<Option<String>, &'static str> {
    let Some(raw) = std::env::var_os("PUNKTFUNK_SEAT_ID") else {
        // A seat without an id would mint audio devnodes the console host also matches.
        if is_seat_host() {
            return Err("PUNKTFUNK_SEAT_SESSION=1 without a PUNKTFUNK_SEAT_ID");
        }
        return Ok(None);
    };
    let text = raw
        .to_str()
        .ok_or("PUNKTFUNK_SEAT_ID must be valid Unicode")?;
    validate_seat_id(text).map(|id| Some(id.to_owned()))
}

/// The box's config dir a seat host reads its trust from (`PUNKTFUNK_TRUST_DIR`): the
/// identity, the pairing store, `profiles.json` and the device display overlays. Read only;
/// the box host is their one writer. `None` on the box host, and for a relative path.
pub fn trust_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("PUNKTFUNK_TRUST_DIR")
        .map(std::path::PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

/// The box's config dir a Windows seat host reads the library from (`PUNKTFUNK_LIBRARY_DIR`):
/// `library*.json`, `library-metadata/`, and the plugin manifests and grants an `exec` entry
/// resolves against. Read only; the box host is their one writer. `None` on every other host,
/// and for a relative path.
pub fn box_library_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("PUNKTFUNK_LIBRARY_DIR")
        .map(std::path::PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

/// A Windows seat's own `steam.exe` (`PUNKTFUNK_SEAT_STEAM`), which may not be copied yet. A seat
/// host starts Steam from nowhere else. `None` on every other host, and for a relative path.
pub fn seat_steam() -> Option<std::path::PathBuf> {
    std::env::var_os("PUNKTFUNK_SEAT_STEAM")
        .map(std::path::PathBuf::from)
        .filter(|exe| exe.is_absolute())
}

/// Where this host reads its library: [`box_library_dir`], else its own config dir. Play stats
/// are never here; they stay in [`crate::config_dir`].
pub fn library_dir() -> std::path::PathBuf {
    box_library_dir().unwrap_or_else(crate::config_dir)
}

/// `PUNKTFUNK_PAIRING=refused`: devices pair with the box, never with this host. A knock is
/// refused, a PIN window never opens.
pub fn pairing_refused() -> bool {
    std::env::var("PUNKTFUNK_PAIRING").as_deref() == Ok("refused")
}

/// `PUNKTFUNK_SEAT_OWNER=1`: this seat host is the box owner's own, behind the door. It serves
/// the owner profile and the light-seat profiles that play inside the owner's host.
pub fn is_owner_seat() -> bool {
    is_seat_host() && std::env::var("PUNKTFUNK_SEAT_OWNER").as_deref() == Ok("1")
}

/// A seat host that is not the owner's: its desktop follows the seat contract — one virtual
/// screen, never a real monitor, the box's Seat defaults and no settings of its own
/// (`design/web-console-structure-2026-10.md` §2.1).
pub fn follows_contract() -> bool {
    is_seat_host() && !is_owner_seat()
}

/// Whether this host runs a seat's own session, whose Game Mode is the host's: a seat's, or the
/// owner's row in the headless session the seat runner starts it in
/// (`PUNKTFUNK_SEAT_HEADLESS=1`). The owner's own host at the desk reads the row's contract too,
/// but the box's session there is the owner's.
pub fn runs_seat_session() -> bool {
    follows_contract()
        || (is_owner_seat() && std::env::var("PUNKTFUNK_SEAT_HEADLESS").as_deref() == Ok("1"))
}

/// The seat supervisor's pad broker: a seat host asks here for the pads it cannot make itself.
pub const PADS_SOCKET: &str = "/run/punktfunk/pads.sock";

/// The supervisor's uinput proxy for `account`, a CUSE device under `/dev`; the seat's unit
/// binds it over `/dev/uinput`. Sysfs names it with `!` for each `/`.
pub fn uinput_devname(account: &str) -> String {
    format!("punktfunk/uinput-{account}")
}

static DOOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marks this process as the door (`serve --door`).
pub fn set_door() {
    DOOR.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether this host is the door: it advertises, pairs, serves the console and places every
/// connect on a seat, and never streams itself. `serve --door` or `PUNKTFUNK_DOOR=1`.
pub fn is_door() -> bool {
    DOOR.load(std::sync::atomic::Ordering::Relaxed)
        || std::env::var("PUNKTFUNK_DOOR").as_deref() == Ok("1")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What each predicate answers, by the markers set. Inputs: `S` `PUNKTFUNK_SEAT_SESSION=1`,
    /// `O` `PUNKTFUNK_SEAT_OWNER=1`, `H` `PUNKTFUNK_SEAT_HEADLESS=1`, `D` the door (`serve --door`
    /// or `PUNKTFUNK_DOOR=1`), `P` `PUNKTFUNK_PAIRING=refused`. Answers, in [`answers`]' order:
    /// `s` seat host, `o` owner seat, `c` follows the contract, `r` runs a seat session, `d` door,
    /// `p` pairing refused, `e` a missing seat id is an error.
    const TRUTH: [(&str, &str); 32] = [
        ("-----", "-------"),
        ("----P", "-----p-"),
        ("---D-", "----d--"),
        ("---DP", "----dp-"),
        ("--H--", "-------"),
        ("--H-P", "-----p-"),
        ("--HD-", "----d--"),
        ("--HDP", "----dp-"),
        ("-O---", "-------"),
        ("-O--P", "-----p-"),
        ("-O-D-", "----d--"),
        ("-O-DP", "----dp-"),
        ("-OH--", "-------"),
        ("-OH-P", "-----p-"),
        ("-OHD-", "----d--"),
        ("-OHDP", "----dp-"),
        ("S----", "s-cr--e"),
        ("S---P", "s-cr-pe"),
        ("S--D-", "s-crd-e"),
        ("S--DP", "s-crdpe"),
        ("S-H--", "s-cr--e"),
        ("S-H-P", "s-cr-pe"),
        ("S-HD-", "s-crd-e"),
        ("S-HDP", "s-crdpe"),
        ("SO---", "so----e"),
        ("SO--P", "so---pe"),
        ("SO-D-", "so--d-e"),
        ("SO-DP", "so--dpe"),
        ("SOH--", "so-r--e"),
        ("SOH-P", "so-r-pe"),
        ("SOHD-", "so-rd-e"),
        ("SOHDP", "so-rdpe"),
    ];

    /// Each role variable in [`TRUTH`]'s input order: its name, the value that sets its marker,
    /// and a value that must not.
    const VARS: [(&str, &str, &str); 5] = [
        ("PUNKTFUNK_SEAT_SESSION", "1", "true"),
        ("PUNKTFUNK_SEAT_OWNER", "1", "0"),
        ("PUNKTFUNK_SEAT_HEADLESS", "1", "yes"),
        ("PUNKTFUNK_DOOR", "1", "true"),
        ("PUNKTFUNK_PAIRING", "refused", "1"),
    ];

    /// Set on the child only: `flag` for a `serve --door` process.
    const CHILD: &str = "PF_ROLE_TRUTH_CHILD";
    const MARK: &str = "role-answers=";
    /// Every variable unset, set and near-missed, with and without the flag.
    const COMBINATIONS: usize = 3 * 3 * 3 * 3 * 3 * 2;

    fn answers() -> String {
        let answers = [
            ('s', is_seat_host()),
            ('o', is_owner_seat()),
            ('c', follows_contract()),
            ('r', runs_seat_session()),
            ('d', is_door()),
            ('p', pairing_refused()),
            ('e', seat_id().is_err()),
        ];
        answers
            .iter()
            .map(|(letter, on)| if *on { *letter } else { '-' })
            .collect()
    }

    /// The predicates read the process environment, so each combination runs this test again as
    /// a child with that environment, and the child prints its answers.
    #[test]
    fn every_predicate_answers_its_truth_table_row() {
        if let Some(door) = std::env::var_os(CHILD) {
            if door == "flag" {
                set_door();
            }
            println!("{MARK}{}", answers());
            return;
        }
        let workers = std::thread::available_parallelism().map_or(1, usize::from);
        std::thread::scope(|scope| {
            for worker in 0..workers {
                scope.spawn(move || (worker..COMBINATIONS).step_by(workers).for_each(check));
            }
        });
    }

    /// Bit 0 of `combination` is the flag; the rest is a base-3 digit per variable.
    fn check(combination: usize) {
        let flag = combination % 2 == 1;
        let test = format!(
            "{}::every_predicate_answers_its_truth_table_row",
            module_path!().split_once("::").expect("crate::module").1
        );
        let mut child =
            std::process::Command::new(std::env::current_exe().expect("this test binary"));
        child
            .args(["--exact", &test, "--nocapture"])
            .env(CHILD, if flag { "flag" } else { "none" })
            .env_remove("PUNKTFUNK_SEAT_ID");
        let mut digits = combination / 2;
        let mut set = [false; 5];
        for (at, (name, on, near_miss)) in VARS.iter().enumerate() {
            match digits % 3 {
                0 => child.env_remove(name),
                1 => child.env(name, on),
                _ => child.env(name, near_miss),
            };
            set[at] = digits % 3 == 1;
            digits /= 3;
        }
        set[3] |= flag;
        let inputs: String = "SOHDP"
            .chars()
            .zip(set)
            .map(|(letter, on)| if on { letter } else { '-' })
            .collect();
        let row = TRUTH.iter().find(|(row, _)| *row == inputs);
        let out = child.output().expect("run the child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let got = stdout.split(MARK).nth(1).and_then(|rest| rest.get(..7));
        assert_eq!(
            got,
            row.map(|(_, answers)| *answers),
            "{inputs}, combination {combination}"
        );
    }

    #[test]
    fn seat_ids_reject_unsafe_marker_and_log_text() {
        for raw in [
            "",
            "550E8400E29B41D4A716446655440000",
            "550e8400-e29b-41d4-a716-446655440000",
            "seat 1",
            "seat/1",
            "seat\n1",
            "séat-1",
            "0123456789abcdefg123456789abcdef",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(validate_seat_id(raw).is_err(), "{raw:?}");
        }
        assert_eq!(
            validate_seat_id("0123456789abcdef0123456789abcdef"),
            Ok("0123456789abcdef0123456789abcdef")
        );
    }
}
