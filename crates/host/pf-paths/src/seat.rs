//! What this host reads of the seat contract, in one place.
//!
//! [`HostRole`] is what the process is: the console host, the door, or a seat's host. The seat
//! supervisor (`pf-seats`) marks each seat host with `PUNKTFUNK_SEAT_SESSION=1` plus a
//! `PUNKTFUNK_SEAT_ID`; the host never learns how those sessions come to exist. No marker is
//! the console host.
//!
//! `serve` fixes the role with [`resolve`]. Any other process reads the environment at each
//! [`role`] call. No other crate reads a role variable.
//! `every_predicate_answers_its_truth_table_row` pins what each role answers.
//!
//! The id is untrusted process input, so it is validated once here rather than
//! at each use: it reaches a device-parameter marker and log lines.
//! `docs-site/content/docs/developers/multi-seat-contract.md` is the contract of record.

/// What this host process is. A policy that turns on more than one role is an exhaustive
/// `match`, so a new variant fails to compile wherever nobody has decided for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostRole {
    /// No marker: the console host.
    Plain,
    /// `serve --door` or `PUNKTFUNK_DOOR=1`: it advertises, pairs, serves the console and places
    /// every connect on a seat, and never streams itself.
    Door,
    /// `PUNKTFUNK_SEAT_SESSION=1`: a supervisor-managed seat owns this host, not the console.
    Seat(SeatRole),
    /// The door and the seat marker at once, which no supervisor starts. It answers as the door
    /// where the door is asked for and as the seat where the seat is.
    DoorSeat(SeatRole),
}

/// Whose seat a seat host serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeatRole {
    /// A seat of its own. Its desktop follows the seat contract: one virtual screen, never a
    /// real monitor, the box's Seat defaults and no settings of its own
    /// (`design/web-console-structure-2026-10.md` §2.1).
    Contract,
    /// `PUNKTFUNK_SEAT_OWNER=1`: the box owner's own host behind the door, at the desk. It
    /// serves the owner profile and the light-seat profiles that play inside the owner's host.
    /// It reads the row's contract, but the box's session there is the owner's.
    Owner,
    /// The owner's row in the headless session the seat runner starts it in
    /// (`PUNKTFUNK_SEAT_HEADLESS=1`). Its Game Mode is the host's, as a seat's is.
    OwnerHeadless,
}

impl HostRole {
    /// Every role, for a test that walks a policy over all of them.
    pub const ALL: [HostRole; 8] = [
        HostRole::Plain,
        HostRole::Door,
        HostRole::Seat(SeatRole::Contract),
        HostRole::Seat(SeatRole::Owner),
        HostRole::Seat(SeatRole::OwnerHeadless),
        HostRole::DoorSeat(SeatRole::Contract),
        HostRole::DoorSeat(SeatRole::Owner),
        HostRole::DoorSeat(SeatRole::OwnerHeadless),
    ];

    /// The role these values name. Each is a variable's value, `None` when unset, and a marker
    /// is its exact value `1`. `door_flag` is `serve --door`. The owner and headless markers
    /// mean nothing without the seat marker.
    pub fn from_env_values(
        seat_session: Option<&str>,
        seat_owner: Option<&str>,
        seat_headless: Option<&str>,
        door: Option<&str>,
        door_flag: bool,
    ) -> HostRole {
        let on = |value: Option<&str>| value == Some("1");
        let seat = match (on(seat_session), on(seat_owner), on(seat_headless)) {
            (false, _, _) => None,
            (true, false, _) => Some(SeatRole::Contract),
            (true, true, false) => Some(SeatRole::Owner),
            (true, true, true) => Some(SeatRole::OwnerHeadless),
        };
        match (seat, door_flag || on(door)) {
            (None, false) => HostRole::Plain,
            (None, true) => HostRole::Door,
            (Some(seat), false) => HostRole::Seat(seat),
            (Some(seat), true) => HostRole::DoorSeat(seat),
        }
    }

    fn seat(self) -> Option<SeatRole> {
        match self {
            HostRole::Plain | HostRole::Door => None,
            HostRole::Seat(seat) | HostRole::DoorSeat(seat) => Some(seat),
        }
    }

    /// Whether a supervisor-managed seat owns this host rather than the console.
    pub fn is_seat_host(self) -> bool {
        self.seat().is_some()
    }

    /// Whether this seat host is the box owner's own, behind the door.
    pub fn is_owner_seat(self) -> bool {
        match self.seat() {
            Some(SeatRole::Owner | SeatRole::OwnerHeadless) => true,
            Some(SeatRole::Contract) | None => false,
        }
    }

    /// Whether this is a seat host that is not the owner's, whose desktop follows the seat
    /// contract ([`SeatRole::Contract`]).
    pub fn follows_contract(self) -> bool {
        match self.seat() {
            Some(SeatRole::Contract) => true,
            Some(SeatRole::Owner | SeatRole::OwnerHeadless) | None => false,
        }
    }

    /// Whether this host runs a seat's own session, whose Game Mode is the host's: a seat's, or
    /// the owner's row in its headless session. Not the owner's host at the desk.
    pub fn runs_seat_session(self) -> bool {
        match self.seat() {
            Some(SeatRole::Contract | SeatRole::OwnerHeadless) => true,
            Some(SeatRole::Owner) | None => false,
        }
    }

    /// Whether this host is the door.
    pub fn is_door(self) -> bool {
        match self {
            HostRole::Door | HostRole::DoorSeat(_) => true,
            HostRole::Plain | HostRole::Seat(_) => false,
        }
    }
}

/// The role and whether pairing is refused, as [`resolve`] fixed them.
static RESOLVED: std::sync::OnceLock<(HostRole, bool)> = std::sync::OnceLock::new();

/// The role variables as they are now. `door_flag` is `serve --door`.
fn read(door_flag: bool) -> (HostRole, bool) {
    let var = |name: &str| std::env::var(name).ok();
    let role = HostRole::from_env_values(
        var("PUNKTFUNK_SEAT_SESSION").as_deref(),
        var("PUNKTFUNK_SEAT_OWNER").as_deref(),
        var("PUNKTFUNK_SEAT_HEADLESS").as_deref(),
        var("PUNKTFUNK_DOOR").as_deref(),
        door_flag,
    );
    (role, var("PUNKTFUNK_PAIRING").as_deref() == Some("refused"))
}

/// Fixes this process's role for the rest of its life. `serve` calls it once its arguments are
/// read, with whether `--door` is among them. The first call wins.
pub fn resolve(door_flag: bool) {
    RESOLVED.get_or_init(|| read(door_flag));
}

/// What [`resolve`] fixed, else the environment as it is now. A process that never resolves (a
/// management CLI, the Windows service, a test) reads it at each call.
fn current() -> (HostRole, bool) {
    RESOLVED.get().copied().unwrap_or_else(|| read(false))
}

/// This process's role: what [`resolve`] fixed, else the environment as it is now.
pub fn role() -> HostRole {
    current().0
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
        if role().is_seat_host() {
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
/// refused, a PIN window never opens. [`resolve`] fixes it with the role.
pub fn pairing_refused() -> bool {
    current().1
}

/// The seat supervisor's pad broker: a seat host asks here for the pads it cannot make itself.
pub const PADS_SOCKET: &str = "/run/punktfunk/pads.sock";

/// The supervisor's uinput proxy for `account`, a CUSE device under `/dev`; the seat's unit
/// binds it over `/dev/uinput`. Sysfs names it with `!` for each `/`.
pub fn uinput_devname(account: &str) -> String {
    format!("punktfunk/uinput-{account}")
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
        let role = role();
        let answers = [
            ('s', role.is_seat_host()),
            ('o', role.is_owner_seat()),
            ('c', role.follows_contract()),
            ('r', role.runs_seat_session()),
            ('d', role.is_door()),
            ('p', pairing_refused()),
            ('e', seat_id().is_err()),
        ];
        answers
            .iter()
            .map(|(letter, on)| if *on { *letter } else { '-' })
            .collect()
    }

    /// The role comes from the process environment, so each combination runs this test again as
    /// a child with that environment. The child prints its answers twice: read from the
    /// environment, then as [`resolve`] fixes them.
    #[test]
    fn every_predicate_answers_its_truth_table_row() {
        if let Some(door) = std::env::var_os(CHILD) {
            println!("{MARK}{}", answers());
            resolve(door == "flag");
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
        let row = |door_flag: bool| {
            let mut set = set;
            set[3] |= door_flag;
            let inputs: String = "SOHDP"
                .chars()
                .zip(set)
                .map(|(letter, on)| if on { letter } else { '-' })
                .collect();
            let row = TRUTH.iter().find(|(row, _)| *row == inputs);
            row.map(|(_, answers)| *answers)
        };
        let out = child.output().expect("run the child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let got: Vec<_> = stdout
            .split(MARK)
            .skip(1)
            .map(|rest| rest.get(..7))
            .collect();
        // Only `resolve` knows the flag.
        assert_eq!(
            got,
            [row(false), row(flag)],
            "combination {combination}, markers {set:?}"
        );
    }

    /// A policy test walks [`HostRole::ALL`], so it holds each role the markers name, once.
    #[test]
    fn all_holds_each_role_the_markers_name() {
        let mut named = Vec::new();
        for markers in 0..16u8 {
            let value = |bit: u8| (markers & bit != 0).then_some("1");
            let role = HostRole::from_env_values(value(1), value(2), value(4), value(8), false);
            if !named.contains(&role) {
                named.push(role);
            }
        }
        assert_eq!(named.len(), HostRole::ALL.len());
        assert!(HostRole::ALL.iter().all(|role| named.contains(role)));
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
