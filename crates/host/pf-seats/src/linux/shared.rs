//! What seats share and what each keeps: the games folder, the trust copy and the Steam clone.
//!
//! Under the supervisor's state directory ([`super::STATE_DIR`]), root's alone:
//!
//! - `games/steamapps/{common,compatdata,shadercache,downloading}` is one library every seat
//!   writes. `compatdata`, `shadercache` and `downloading` are covered per seat by bind mounts
//!   (`session.rs`), so Wine prefixes and download state never cross.
//! - `seats/<id>/` holds the seat user's home, those three private directories, and the box
//!   owner's libraries as the seat sees them (`libraries.rs`).
//! - `trust/<id>/` is a read-only copy of the box's pairing store and profiles from the door's
//!   directory, so a seat user can read what it needs and nothing else there.
//! - `seats/hosts/<id>/mgmt-token` is the token the box presents to that seat host's loopback
//!   API. It belongs to the `punktfunk` user, not the seat group: every seat user is a member.

use super::accounts::Passwd;
use super::session::PRIVATE_DIRS;
use super::{err, run, safe_open};
use crate::backend::BackendError;
use crate::model::SeatId;
use rustix::fs::{AtFlags, Mode, OFlags};
use std::collections::HashSet;
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};

/// Box files a seat host reads; any may not exist yet. Never the box's identity: a seat host
/// mints its own, and clients reach it through the pin the box's `Redirect` carries.
const TRUST_FILES: [&str; 4] = [
    "punktfunk1-paired.json",
    "profiles.json",
    "display-settings.json",
    // `pf_host_config::seat_defaults::FILE`: the Seat defaults a seat applies itself.
    "seat-defaults.json",
];

/// The box's identity pair, which an older trust copy carried. Removed from every copy.
const RETIRED: [&str; 2] = ["native-cert.pem", "native-key.pem"];

/// Profile avatars, one flat directory beside `profiles.json`.
const AVATARS: &str = "profiles";

/// A trust file larger than this is not ours.
pub(super) const MAX_TRUST_BYTES: u64 = 8 * 1024 * 1024;

/// Steam top-level entries a seat does not inherit: the owner's account, library and logs.
const CLONE_SKIP: &[&str] = &["config", "userdata", "appcache", "steamapps", "logs"];

/// Steam's launcher script. A clone carries it and a self-bootstrapped Steam does too, so its
/// presence says the seat already has an install.
const STEAM_MARKER: &str = ".local/share/Steam/steam.sh";

pub(super) fn seat_dir(state: &Path, id: &SeatId) -> PathBuf {
    state.join("seats").join(id.as_str())
}

pub(super) fn trust_dir(state: &Path, id: &SeatId) -> PathBuf {
    state.join("trust").join(id.as_str())
}

pub(super) fn games_dir(state: &Path) -> PathBuf {
    state.join("games")
}

/// The ledger's directory, `seats` under the state directory: `persistence::default_root()` on
/// Linux. Seat directories are named by id, so they never collide with the ledger's files.
pub fn ledger_root(state: &Path) -> PathBuf {
    state.join("seats")
}

fn hosts_dir(state: &Path) -> PathBuf {
    ledger_root(state).join("hosts")
}

/// Where the box reads a seat host's management token.
pub(super) fn host_dir(state: &Path, id: &SeatId) -> PathBuf {
    hosts_dir(state).join(id.as_str())
}

/// The owner of a seat's token file: the `punktfunk` user's `(uid, gid)` when it exists, else
/// root, so the box host reads it as itself and no seat user can.
pub(super) fn token_owner(punktfunk: Option<&Passwd>) -> (u32, u32) {
    punktfunk.map_or((0, 0), |user| (user.uid, user.gid))
}

/// Writes `line` as the seat's token file, `0600` in a `0700` directory, both owned by `owner`.
pub(super) fn write_host_token(
    state: &Path,
    id: &SeatId,
    line: &str,
    owner: (u32, u32),
) -> Result<(), BackendError> {
    let io = |what: &str, e| io_err("host_token", what, e);
    make_dir(&hosts_dir(state), 0o711).map_err(|e| io("create the hosts directory", e))?;
    let dir = host_dir(state, id);
    make_dir(&dir, 0o700).map_err(|e| io("create the token directory", e))?;
    std::os::unix::fs::chown(&dir, Some(owner.0), Some(owner.1))
        .map_err(|e| io("own the token directory", e))?;
    let file = dir.join("mgmt-token");
    let temp = dir.join("mgmt-token.tmp");
    let _ = std::fs::remove_file(&temp);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| io("create the token file", e))?;
    out.write_all(line.as_bytes())
        .map_err(|e| io("write the token file", e))?;
    std::os::unix::fs::fchown(&out, Some(owner.0), Some(owner.1))
        .map_err(|e| io("own the token file", e))?;
    drop(out);
    std::fs::rename(temp, file).map_err(|e| io("replace the token file", e))
}

fn io_err(code: &str, what: &str, error: std::io::Error) -> BackendError {
    err(code, format!("{what}: {error}"))
}

/// Creates `path` and its parents, then sets `mode` on `path` itself.
pub(super) fn make_dir(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// The state directory and the directories every seat needs below it, created `0711` if absent:
/// a seat user walks to its own entry and can't list the others. Each must be a directory this
/// process owns and no group or other may write; `games`, which every seat writes, only no other.
pub(super) fn ensure_layout(state: &Path) -> std::io::Result<()> {
    for dir in [
        state.to_path_buf(),
        ledger_root(state),
        hosts_dir(state),
        state.join("trust"),
    ] {
        match std::fs::DirBuilder::new().mode(0o711).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        check_owned(&dir, 0o022)?;
    }
    match check_owned(&games_dir(state), 0o002) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        checked => checked,
    }
}

/// Refuses `path` unless it is a directory, not a link, owned by this process's uid, with none
/// of the `forbidden` mode bits set.
fn check_owned(path: &Path, forbidden: u32) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    let ours = meta.uid() == rustix::process::geteuid().as_raw();
    if !meta.is_dir() || !ours || meta.mode() & forbidden != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a directory only the supervisor's uid can change",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// The shared games library: group `punktfunk`, setgid, with a default ACL so a file one seat
/// creates is writable by the others.
pub(super) fn ensure_games(state: &Path, gid: u32) -> Result<(), BackendError> {
    let games = games_dir(state);
    let steamapps = games.join("steamapps");
    let mut dirs = vec![games, steamapps.clone()];
    for name in ["common", "compatdata", "shadercache", "downloading"] {
        dirs.push(steamapps.join(name));
    }
    for dir in &dirs {
        let io = |what: &str, e| io_err("games_dir", &format!("{what} {}", dir.display()), e);
        std::fs::create_dir_all(dir).map_err(|e| io("create", e))?;
        std::os::unix::fs::chown(dir, Some(0), Some(gid)).map_err(|e| io("own", e))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o2775))
            .map_err(|e| io("set the mode of", e))?;
    }
    let group = super::accounts::GAMES_GROUP;
    let mut args = vec!["-m".to_owned(), format!("g:{group}:rwX,d:g:{group}:rwX")];
    args.extend(dirs.iter().map(|d| d.to_string_lossy().into_owned()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run("setfacl_failed", "setfacl", &args).map(drop)
}

/// Re-applies the ACL mask below the games folder. A file one seat creates with mode 0644 or 0755
/// gets a mask of `r--` or `r-x` that cuts the group entry, so the other seats can't write it.
///
/// Walks the whole tree each call: cheap for a normal library, slow past millions of files,
/// where an inotify watch on `steamapps` would fix only what changed.
pub(super) fn fix_acl(state: &Path) -> Result<(), BackendError> {
    let games = games_dir(state);
    run(
        "setfacl_failed",
        "setfacl",
        &["-R", "-m", "m::rwX", &games.to_string_lossy()],
    )
    .map(drop)
}

/// The seat's private directories, owned by its user and in place before its unit starts: a
/// missing bind source fails the unit with `226/NAMESPACE`.
pub(super) fn ensure_seat_dirs(
    state: &Path,
    id: &SeatId,
    passwd: &Passwd,
) -> Result<(), BackendError> {
    for name in PRIVATE_DIRS {
        let dir = seat_dir(state, id).join(name);
        let io = |what: &str, e| io_err("seat_dirs", &format!("{what} {}", dir.display()), e);
        make_dir(&dir, 0o700).map_err(|e| io("create", e))?;
        std::os::unix::fs::chown(&dir, Some(passwd.uid), Some(passwd.gid))
            .map_err(|e| io("own", e))?;
    }
    Ok(())
}

/// Removes the seat's directories, its trust copy and its token. Missing ones are fine.
pub(super) fn remove_seat_dirs(state: &Path, id: &SeatId) -> Result<(), BackendError> {
    for dir in [
        seat_dir(state, id),
        trust_dir(state, id),
        host_dir(state, id),
    ] {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("seat_dirs", &format!("remove {}", dir.display()), e)),
        }
    }
    Ok(())
}

/// Whether a copy is stale: absent, or a different size or modification time. Copies keep the
/// source's mtime, so an unchanged file compares equal.
fn needs_copy(src: Stamp, dst: Option<Stamp>) -> bool {
    dst != Some(src)
}

/// Modification time in seconds and nanoseconds, then size.
type Stamp = (i64, i64, u64);

// The field types differ by target: a cast that is a no-op on one is needed on another.
#[allow(clippy::unnecessary_cast)]
fn stamp(st: &rustix::fs::Stat) -> Stamp {
    (
        st.st_mtime as i64,
        st.st_mtime_nsec as i64,
        st.st_size as u64,
    )
}

/// Who reads a trust copy. A seat user reads through the group, `root:gid 0640`. The owner's own
/// account may have a shared primary group, so it reads as the file's owner, `uid:gid 0600`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Reader {
    uid: Option<u32>,
    gid: u32,
}

impl Reader {
    pub(super) fn group(gid: u32) -> Self {
        Self { uid: None, gid }
    }

    pub(super) fn person(uid: u32, gid: u32) -> Self {
        Self {
            uid: Some(uid),
            gid,
        }
    }

    fn file_mode(self) -> u32 {
        if self.uid.is_some() {
            0o600
        } else {
            0o640
        }
    }

    fn dir_mode(self) -> u32 {
        if self.uid.is_some() {
            0o700
        } else {
            0o750
        }
    }
}

/// Removes `name` from `dir`; `true` when there was one.
fn unlink(dir: &OwnedFd, name: &str) -> std::io::Result<bool> {
    match rustix::fs::unlinkat(dir, name, AtFlags::empty()) {
        Ok(()) => Ok(true),
        Err(e) if e == rustix::io::Errno::NOENT => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Copies `name` from the box's `src` into `dst` for `reader` when it differs, and removes it from
/// `dst` when the box has none. Returns whether `dst` changed. Both are directory handles, so no
/// link on either side is followed, whoever owns the directory.
fn sync_file(
    src: Option<&OwnedFd>,
    dst: &OwnedFd,
    name: &str,
    reader: Reader,
) -> std::io::Result<bool> {
    let found = match src {
        Some(src) => safe_open::read_at(src, name, MAX_TRUST_BYTES)?,
        None => None,
    };
    let Some((bytes, meta)) = found else {
        return unlink(dst, name);
    };
    let wanted = (meta.mtime(), meta.mtime_nsec(), meta.size());
    let current = rustix::fs::statat(dst, name, AtFlags::SYMLINK_NOFOLLOW)
        .ok()
        .map(|st| stamp(&st));
    if !needs_copy(wanted, current) {
        return Ok(false);
    }
    let temp = format!("{name}.tmp");
    unlink(dst, &temp)?;
    let create =
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mode = Mode::from_raw_mode(reader.file_mode() as _);
    let mut out = std::fs::File::from(rustix::fs::openat(dst, temp.as_str(), create, mode)?);
    out.write_all(&bytes)?;
    out.set_permissions(std::fs::Permissions::from_mode(reader.file_mode()))?;
    std::os::unix::fs::fchown(&out, reader.uid, Some(reader.gid))?;
    out.set_modified(meta.modified()?)?;
    drop(out);
    rustix::fs::renameat(dst, temp.as_str(), dst, name)?;
    Ok(true)
}

/// Brings the seat's trust copy level with the box's files in `box_dir`, without the box's
/// identity: a game on the seat could otherwise answer as the box. Returns whether anything
/// changed. The door owns `box_dir` and the owner owns their own copy, so both are read and
/// written through directory handles.
pub(super) fn refresh_trust(
    box_dir: &Path,
    state: &Path,
    id: &SeatId,
    reader: Reader,
) -> Result<bool, BackendError> {
    let dst = trust_dir(state, id);
    let io = |what: &str, e| io_err("trust_copy", what, e);
    make_dir(&dst, reader.dir_mode()).map_err(|e| io("create the trust copy", e))?;
    std::os::unix::fs::chown(&dst, reader.uid, Some(reader.gid))
        .map_err(|e| io("own the trust copy", e))?;
    let dst = safe_open::dir(&dst).map_err(|e| io("open the trust copy", e))?;
    let src = match safe_open::dir(box_dir) {
        Ok(src) => Some(src),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io("open the box directory", e)),
    };
    let mut changed = false;
    for name in RETIRED {
        changed |= unlink(&dst, name).map_err(|e| io(&format!("remove the old {name}"), e))?;
    }
    for name in TRUST_FILES {
        changed |= sync_file(src.as_ref(), &dst, name, reader)
            .map_err(|e| io(&format!("copy {name}"), e))?;
    }
    changed |= sync_avatars(src.as_ref(), &dst, reader).map_err(|e| io("copy the avatars", e))?;
    Ok(changed)
}

/// The names in `dir`, without `.` and `..`; a name that is not UTF-8 is left out.
fn names(dir: &OwnedFd) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in rustix::fs::Dir::read_from(dir)? {
        if let Ok(name) = entry?.file_name().to_str()
            && !matches!(name, "." | "..")
        {
            out.push(name.to_owned());
        }
    }
    Ok(out)
}

/// Mirrors the box's flat avatar directory into `dst`: copies each file, drops copies whose
/// source is gone. A link where the directory should be is refused.
fn sync_avatars(src: Option<&OwnedFd>, dst: &OwnedFd, reader: Reader) -> std::io::Result<bool> {
    let src = match src {
        Some(src) => safe_open::dir_at(src, AVATARS)?,
        None => None,
    };
    if src.is_some() {
        let mode = Mode::from_raw_mode(reader.dir_mode() as _);
        match rustix::fs::mkdirat(dst, AVATARS, mode) {
            Ok(()) => {}
            Err(e) if e == rustix::io::Errno::EXIST => {}
            Err(e) => return Err(e.into()),
        }
    }
    let Some(copies) = safe_open::dir_at(dst, AVATARS)? else {
        return Ok(false);
    };
    let mut wanted = HashSet::new();
    let mut changed = false;
    if let Some(src) = &src {
        rustix::fs::fchmod(&copies, Mode::from_raw_mode(reader.dir_mode() as _))?;
        std::os::unix::fs::fchown(&copies, reader.uid, Some(reader.gid))?;
        for name in names(src)?.into_iter().filter(|n| safe_name(n)) {
            // A link or oversized entry is skipped, not copied and not fatal.
            if let Ok(did) = sync_file(Some(src), &copies, &name, reader) {
                changed |= did;
                wanted.insert(name);
            }
        }
    }
    for name in names(&copies)? {
        if !wanted.contains(&name) {
            changed |= unlink(&copies, &name)?;
        }
    }
    if src.is_none() {
        let _ = rustix::fs::unlinkat(dst, AVATARS, AtFlags::REMOVEDIR);
    }
    Ok(changed)
}

/// A file name that is one plain path component: letters, digits, `.`, `_` and `-`.
fn safe_name(name: &str) -> bool {
    !matches!(name, "." | "..")
        && !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Top-level Steam entries the seat does not get.
fn excluded_from_clone(name: &str) -> bool {
    CLONE_SKIP.contains(&name) || name.ends_with(".vdf")
}

/// The box owner's Steam install: `~/.steam/steam`, else `~/.local/share/Steam`.
fn steam_root_of(home: &Path) -> Option<PathBuf> {
    let linked = home.join(".steam/steam");
    if linked.is_dir() {
        return Some(std::fs::canonicalize(&linked).unwrap_or(linked));
    }
    let data = home.join(".local/share/Steam");
    data.is_dir().then_some(data)
}

/// The Steam a new seat is cloned from: the lowest-uid ordinary user that has one. A box with
/// several Steam users picks the first; `--steam-source` names another.
pub(super) fn find_steam_source(users: &[Passwd]) -> Option<PathBuf> {
    let mut ordinary: Vec<&Passwd> = users
        .iter()
        .filter(|u| (1000..60000).contains(&u.uid) && !u.gecos.starts_with("punktfunk-seat="))
        .collect();
    ordinary.sort_by_key(|u| u.uid);
    ordinary.into_iter().find_map(|u| steam_root_of(&u.home))
}

/// A Steam under the seat's home, cloned from `source` when there is one, and a library list that
/// names the shared games folder first. Never fails the seat: without a clone Steam downloads
/// itself on first run, and without the list it offers games again.
pub(super) fn provision_steam(state: &Path, passwd: &Passwd, source: Option<&Path>) {
    let steam = passwd.home.join(".local/share/Steam");
    if !passwd.home.join(STEAM_MARKER).exists() {
        match source {
            Some(src) => {
                if let Err(error) = clone_install(src, &steam) {
                    let _ = std::fs::remove_dir_all(&steam);
                    tracing::warn!(%error, "steam clone did not complete; the seat downloads its own");
                }
            }
            None => tracing::info!("no steam install to clone; the seat downloads its own"),
        }
    }
    if let Err(error) = write_library_folders(state, &steam) {
        tracing::warn!(%error, "steam library list not written");
    }
    let local = passwd.home.join(".local");
    let owner = format!("{}:{}", passwd.uid, passwd.gid);
    if let Err(error) = run(
        "chown_failed",
        "chown",
        &["-R", &owner, &local.to_string_lossy()],
    ) {
        tracing::warn!(%error, "steam files not handed to the seat user");
    }
}

/// `cp -a --reflink=auto` of every non-skipped top-level entry. A filesystem without reflinks
/// makes a real copy, which takes a while but leaves the seat a working Steam.
fn clone_install(src: &Path, dst: &Path) -> Result<(), BackendError> {
    let mut args = vec!["-a".to_owned(), "--reflink=auto".to_owned()];
    let entries = std::fs::read_dir(src).map_err(|e| io_err("steam_clone", "read steam", e))?;
    for entry in entries.flatten() {
        if !excluded_from_clone(&entry.file_name().to_string_lossy()) {
            args.push(entry.path().to_string_lossy().into_owned());
        }
    }
    if args.len() == 2 {
        return Err(err(
            "steam_clone",
            "the steam install holds nothing to clone",
        ));
    }
    std::fs::create_dir_all(dst).map_err(|e| io_err("steam_clone", "create the seat steam", e))?;
    args.push(dst.to_string_lossy().into_owned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run("steam_clone", "cp", &args).map(drop)
}

/// Lists the shared games folder first, then the seat's own Steam. Written once; Steam owns the
/// file afterwards.
fn write_library_folders(state: &Path, steam: &Path) -> std::io::Result<()> {
    let dir = steam.join("steamapps");
    let file = dir.join("libraryfolders.vdf");
    if file.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    let folders = [
        games_dir(state).to_string_lossy().into_owned(),
        steam.to_string_lossy().into_owned(),
    ];
    std::fs::write(file, crate::steam::library_folders_vdf(&folders))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> SeatId {
        SeatId::parse("0123456789abcdef0123456789abcdef").unwrap()
    }

    fn own_gid(dir: &Path) -> u32 {
        std::fs::metadata(dir).unwrap().gid()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn box_with_identity(dir: &Path) {
        std::fs::write(dir.join("native-cert.pem"), "cert").unwrap();
        std::fs::write(dir.join("native-key.pem"), "key").unwrap();
    }

    #[test]
    fn the_account_and_the_library_stay_with_the_owner() {
        for skipped in [
            "config",
            "userdata",
            "appcache",
            "steamapps",
            "logs",
            "local.vdf",
            "loginusers.vdf",
        ] {
            assert!(excluded_from_clone(skipped), "{skipped}");
        }
        for kept in [
            "ubuntu12_32",
            "steam.sh",
            "linux64",
            "package",
            "compatibilitytools.d",
        ] {
            assert!(!excluded_from_clone(kept), "{kept}");
        }
    }

    #[test]
    fn a_copy_is_made_only_for_a_missing_or_different_file() {
        assert!(needs_copy((10, 0, 5), None));
        assert!(needs_copy((10, 0, 5), Some((10, 0, 6))));
        assert!(needs_copy((10, 0, 5), Some((11, 0, 5))));
        assert!(needs_copy((10, 0, 5), Some((10, 1, 5))));
        assert!(!needs_copy((10, 0, 5), Some((10, 0, 5))));
    }

    #[test]
    fn the_trust_copy_follows_the_box_and_is_group_readable_only() {
        let temp = tempfile::tempdir().unwrap();
        let box_dir = temp.path();
        let state_temp = tempfile::tempdir().unwrap();
        let state = state_temp.path();
        let gid = own_gid(box_dir);
        box_with_identity(box_dir);
        std::fs::write(box_dir.join("profiles.json"), "{}").unwrap();
        std::fs::create_dir(box_dir.join("profiles")).unwrap();
        std::fs::write(box_dir.join("profiles/p1.png"), "png").unwrap();
        std::fs::write(box_dir.join("mgmt-token"), "secret").unwrap();
        // A copy made before seat hosts minted their own identity.
        let trust = trust_dir(state, &id());
        std::fs::create_dir_all(&trust).unwrap();
        std::fs::write(trust.join("native-key.pem"), "key").unwrap();

        assert!(refresh_trust(box_dir, state, &id(), Reader::group(gid)).unwrap());
        assert_eq!(
            std::fs::read_to_string(trust.join("profiles/p1.png")).unwrap(),
            "png"
        );
        assert_eq!(mode(&trust), 0o750);
        assert_eq!(mode(&trust.join("profiles.json")), 0o640);
        for kept in ["mgmt-token", "native-key.pem", "native-cert.pem"] {
            assert!(!trust.join(kept).exists(), "{kept} stays the box's");
        }
        assert!(
            !refresh_trust(box_dir, state, &id(), Reader::group(gid)).unwrap(),
            "unchanged is a no-op"
        );

        // A changed file, a new avatar and a removed avatar all follow.
        std::fs::write(box_dir.join("profiles.json"), "{\"a\":1}").unwrap();
        std::fs::write(box_dir.join("profiles/p2.png"), "png2").unwrap();
        std::fs::remove_file(box_dir.join("profiles/p1.png")).unwrap();
        assert!(refresh_trust(box_dir, state, &id(), Reader::group(gid)).unwrap());
        assert_eq!(
            std::fs::read_to_string(trust.join("profiles.json")).unwrap(),
            "{\"a\":1}"
        );
        assert!(trust.join("profiles/p2.png").exists());
        assert!(!trust.join("profiles/p1.png").exists());

        // A file the box drops is dropped from the copy.
        std::fs::remove_file(box_dir.join("profiles.json")).unwrap();
        assert!(refresh_trust(box_dir, state, &id(), Reader::group(gid)).unwrap());
        assert!(!trust.join("profiles.json").exists());
    }

    /// The owner's copy is theirs alone: owned by the account, closed to every group, because a
    /// person's primary group may be one everybody on the box shares.
    #[test]
    fn the_owner_s_trust_copy_is_private_to_the_account() {
        let temp = tempfile::tempdir().unwrap();
        let box_dir = temp.path();
        let state_temp = tempfile::tempdir().unwrap();
        let state = state_temp.path();
        box_with_identity(box_dir);
        std::fs::create_dir(box_dir.join("profiles")).unwrap();
        std::fs::write(box_dir.join("profiles/a.png"), "png").unwrap();
        let me = std::fs::metadata(box_dir).unwrap();
        let reader = Reader::person(me.uid(), me.gid());
        assert!(refresh_trust(box_dir, state, &id(), reader).is_ok());
        let trust = trust_dir(state, &id());
        assert_eq!(mode(&trust), 0o700);
        assert_eq!(mode(&trust.join("profiles")), 0o700);
        assert_eq!(mode(&trust.join("profiles/a.png")), 0o600);
    }

    /// A link in the box directory is never followed into the copy.
    #[test]
    fn a_symlink_in_the_box_is_not_copied() {
        let temp = tempfile::tempdir().unwrap();
        let box_dir = temp.path();
        let state_temp = tempfile::tempdir().unwrap();
        let state = state_temp.path();
        box_with_identity(box_dir);
        let secret = box_dir.join("elsewhere");
        std::fs::write(&secret, "root only").unwrap();
        std::os::unix::fs::symlink(&secret, box_dir.join("profiles.json")).unwrap();
        let gid = own_gid(box_dir);
        let error = refresh_trust(box_dir, state, &id(), Reader::group(gid)).unwrap_err();
        assert_eq!(error.code, "trust_copy");
        assert!(!trust_dir(state, &id()).join("profiles.json").exists());
    }

    #[test]
    fn the_token_belongs_to_the_punktfunk_user_when_there_is_one() {
        let user = Passwd {
            name: "punktfunk".into(),
            uid: 972,
            gid: 969,
            gecos: String::new(),
            home: PathBuf::from("/var/lib/punktfunk"),
        };
        assert_eq!(token_owner(Some(&user)), (972, 969));
        assert_eq!(token_owner(None), (0, 0));
    }

    /// The token file is private to its owner and carries the line it was given; a rewrite
    /// replaces it whole; removing the seat takes the directory with it.
    #[test]
    fn a_seat_token_is_private_and_removed_with_the_seat() {
        let temp = tempfile::tempdir().unwrap();
        let box_dir = temp.path();
        let me = std::fs::metadata(box_dir).unwrap();
        let owner = (me.uid(), me.gid());
        write_host_token(box_dir, &id(), "PUNKTFUNK_MGMT_TOKEN=aa\n", owner).unwrap();
        write_host_token(box_dir, &id(), "PUNKTFUNK_MGMT_TOKEN=bb\n", owner).unwrap();
        let dir = host_dir(box_dir, &id());
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&hosts_dir(box_dir)), 0o711);
        assert_eq!(mode(&dir.join("mgmt-token")), 0o600);
        assert_eq!(
            std::fs::read_to_string(dir.join("mgmt-token")).unwrap(),
            "PUNKTFUNK_MGMT_TOKEN=bb\n"
        );
        assert!(!dir.join("mgmt-token.tmp").exists());
        remove_seat_dirs(box_dir, &id()).unwrap();
        assert!(!dir.exists());
    }

    /// The door reads a seat host's token below `persistence::default_root()`.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_ledger_directory_is_the_default_root() {
        assert_eq!(
            ledger_root(Path::new(super::super::STATE_DIR)),
            crate::persistence::default_root()
        );
    }

    #[test]
    fn the_steam_source_is_the_lowest_ordinary_user_with_a_steam() {
        let temp = tempfile::tempdir().unwrap();
        let user = |name: &str, uid: u32, gecos: &str, with_steam: bool| {
            let home = temp.path().join(name);
            if with_steam {
                std::fs::create_dir_all(home.join(".local/share/Steam")).unwrap();
            }
            Passwd {
                name: name.into(),
                uid,
                gid: uid,
                gecos: gecos.into(),
                home,
            }
        };
        let users = [
            user("root", 0, "", true),
            user("seat", 975, "punktfunk-seat=0123", true),
            user("late", 1002, "", true),
            user("bare", 1000, "", false),
            user("early", 1001, "", true),
        ];
        assert_eq!(
            find_steam_source(&users),
            Some(temp.path().join("early/.local/share/Steam"))
        );
        assert_eq!(find_steam_source(&users[..2]), None);
    }
}
