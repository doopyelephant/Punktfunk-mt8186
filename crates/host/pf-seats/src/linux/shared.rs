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
//!   directory, so a seat user can read what it needs and nothing else there. A seat other than
//!   the owner's also finds the box's library there.
//! - `library/` is the box's library: a copy of the owner's, which the owner's host writes. The
//!   door serves it to every client, and each seat's trust copy takes it from here.
//! - `seats/hosts/<id>/mgmt-token` is the token the box presents to that seat host's loopback
//!   API. It belongs to the `punktfunk` user, not the seat group: every seat user is a member.

use super::accounts::Passwd;
use super::session::PRIVATE_DIRS;
use super::{err, run, safe_open};
use crate::backend::BackendError;
use crate::model::SeatId;
use rustix::fs::{AtFlags, Mode, OFlags, XattrFlags};
use rustix::io::Errno;
use std::collections::HashSet;
use std::ffi::CStr;
use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd as _, BorrowedFd, OwnedFd};
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

/// The library files a host reads from its library dir. Play stats stay with each host.
const LIBRARY_FILES: [&str; 5] = [
    "library.json",
    "library-hidden.json",
    "library-metadata.json",
    "library-picks.json",
    "library-scanners.json",
];

/// One table per metadata source, a flat folder beside the library files.
const LIBRARY_METADATA: &str = "library-metadata";

/// A big catalog with descriptions runs to tens of megabytes; past this it is not ours.
const MAX_LIBRARY_BYTES: u64 = 64 * 1024 * 1024;

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

/// The box's library, the door's `PUNKTFUNK_LIBRARY_DIR` (`punktfunk-door.service`).
pub(super) fn box_library_dir(state: &Path) -> PathBuf {
    state.join("library")
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

/// The shared games library: group `punktfunk-games`, setgid, with a default ACL so a file one
/// seat creates is writable by the others. Every seat writes in it, so each folder is opened
/// through the one above it without following a link, and changed through its own handle;
/// `setfacl` gets each as `/proc/<pid>/fd/<n>`, which names that folder whatever its path holds.
pub(super) fn ensure_games(state: &Path, gid: u32) -> Result<(), BackendError> {
    let io = |what: &str, e| io_err("games_dir", what, e);
    let top = safe_open::dir(state).map_err(|e| io("open the state directory", e))?;
    let games = dir_in(&top, "games").map_err(|e| io("open games", e))?;
    let steamapps = dir_in(&games, "steamapps").map_err(|e| io("open games/steamapps", e))?;
    let mut dirs = Vec::new();
    for name in ["common", "compatdata", "shadercache", "downloading"] {
        dirs.push(dir_in(&steamapps, name).map_err(|e| io(&format!("open {name}"), e))?);
    }
    dirs.extend([games, steamapps]);
    for dir in &dirs {
        std::os::unix::fs::fchown(dir, Some(0), Some(gid)).map_err(|e| io("own a folder", e))?;
        rustix::fs::fchmod(dir, Mode::from_raw_mode(0o2775 as _))
            .map_err(|e| io("set the mode of a folder", e.into()))?;
    }
    let group = super::accounts::GAMES_GROUP;
    let pid = std::process::id();
    let mut args = vec!["-m".to_owned(), format!("g:{group}:rwX,d:g:{group}:rwX")];
    args.extend(
        dirs.iter()
            .map(|d| format!("/proc/{pid}/fd/{}", d.as_raw_fd())),
    );
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run("setfacl_failed", "setfacl", &args).map(drop)
}

/// `name` in `at`, created if absent, as a handle; a link there is refused.
fn dir_in(at: &OwnedFd, name: &str) -> std::io::Result<OwnedFd> {
    match rustix::fs::mkdirat(at, name, Mode::from_raw_mode(0o2775 as _)) {
        Ok(()) => {}
        Err(e) if e == rustix::io::Errno::EXIST => {}
        Err(e) => return Err(e.into()),
    }
    safe_open::dir_at(at, name)?.ok_or_else(|| std::io::ErrorKind::NotFound.into())
}

/// The access ACL as the kernel stores it: a 4-byte version 2, then 8-byte entries, little-endian.
const ACL_ACCESS: &str = "system.posix_acl_access";
/// An ACL entry's tag for the mask.
const ACL_MASK: u16 = 0x10;
/// Folders deeper than this below `games` keep their mask: the walk holds a handle per level.
const ACL_DEPTH: usize = 64;

/// Re-applies `m::rwX` below the games folder, as `setfacl -R` would. A file one seat creates
/// with mode 0644 or 0755 gets a mask of `r--` or `r-x` that cuts the group entry, so the other
/// seats can't write it. Every seat writes in the tree, so it is walked through handles: each
/// entry opened in its folder's handle without following a link, and changed through its own.
///
/// Walks the whole tree each call: cheap for a normal library, slow past millions of files,
/// where an inotify watch on `steamapps` would fix only what changed.
pub(super) fn fix_acl(state: &Path) -> Result<(), BackendError> {
    let io = |what: &str, e| io_err("games_acl", what, e);
    let top = safe_open::dir(state).map_err(|e| io("open the state directory", e))?;
    let Some(games) = safe_open::dir_at(&top, "games").map_err(|e| io("open games", e))? else {
        return Ok(());
    };
    mask_tree(games).map_err(|e| io("repair the games ACL mask", e))
}

/// [`fix_acl`]'s walk below `top`, depth first. An entry that goes, or turns into something else
/// mid-walk, is skipped; the first other error ends the walk's answer, not the walk.
fn mask_tree(top: OwnedFd) -> std::io::Result<()> {
    set_mask(&top, true)?;
    let mut first = None;
    let mut stack = vec![rustix::fs::Dir::new(top)?];
    while let Some(dir) = stack.last_mut() {
        let Some(entry) = dir.read() else {
            stack.pop();
            continue;
        };
        let child = entry
            .map_err(std::io::Error::from)
            .and_then(|entry| mask_entry(dir.fd()?, entry.file_name()))
            .and_then(|sub| {
                sub.map(rustix::fs::Dir::new)
                    .transpose()
                    .map_err(Into::into)
            });
        match child {
            Ok(Some(sub)) if stack.len() < ACL_DEPTH => stack.push(sub),
            Ok(_) => {}
            Err(error) => {
                first.get_or_insert(error);
            }
        }
    }
    first.map_or(Ok(()), Err)
}

/// Repairs the mask of `name` in `at`; a folder comes back to be walked. Only a folder or a
/// regular file is opened, never through a link; one swapped for a link or another kind since
/// it was listed is skipped. A FIFO, socket or link has no mask to repair.
fn mask_entry(at: BorrowedFd<'_>, name: &CStr) -> std::io::Result<Option<OwnedFd>> {
    use rustix::fs::FileType;
    if name == c"." || name == c".." {
        return Ok(None);
    }
    let Ok(listed) = rustix::fs::statat(at, name, AtFlags::SYMLINK_NOFOLLOW) else {
        return Ok(None);
    };
    let kind = FileType::from_raw_mode(listed.st_mode);
    let flags = match kind {
        FileType::Directory => OFlags::DIRECTORY,
        FileType::RegularFile => OFlags::NOCTTY,
        _ => return Ok(None),
    };
    let flags = flags | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = match rustix::fs::openat(at, name, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) if [Errno::NOENT, Errno::LOOP, Errno::NOTDIR, Errno::NXIO].contains(&e) => {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let mode = rustix::fs::fstat(&fd)?.st_mode;
    if FileType::from_raw_mode(mode) != kind {
        return Ok(None);
    }
    let dir = kind == FileType::Directory;
    set_mask(&fd, dir || mode & 0o111 != 0)?;
    Ok(dir.then_some(fd))
}

/// Sets the mask of `fd`'s access ACL to `rw`, plus `x` when `exec`. A file with no ACL has no
/// named entry for a mask to cut, and stays as it is.
fn set_mask(fd: impl AsFd, exec: bool) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    let len = match rustix::fs::fgetxattr(&fd, ACL_ACCESS, &mut buf[..]) {
        Ok(len) => len,
        Err(e) if e == Errno::NODATA => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if let Some(acl) = with_mask(&buf[..len], if exec { 7 } else { 6 }) {
        rustix::fs::fsetxattr(&fd, ACL_ACCESS, &acl, XattrFlags::REPLACE)?;
    }
    Ok(())
}

/// `acl` with its mask set to `perm`; `None` when it is no version-2 ACL, has no mask, or its
/// mask is `perm` already.
fn with_mask(acl: &[u8], perm: u16) -> Option<Vec<u8>> {
    let (version, entries) = acl.split_first_chunk::<4>()?;
    if u32::from_le_bytes(*version) != 2 || !entries.len().is_multiple_of(8) {
        return None;
    }
    let mask = entries
        .chunks_exact(8)
        .position(|e| u16::from_le_bytes([e[0], e[1]]) == ACL_MASK)?;
    let at = 4 + mask * 8 + 2;
    if u16::from_le_bytes([acl[at], acl[at + 1]]) == perm {
        return None;
    }
    let mut out = acl.to_vec();
    out[at..at + 2].copy_from_slice(&perm.to_le_bytes());
    Some(out)
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

/// Where the supervisor keeps its copy of a seat host's certificate.
pub(super) fn pin_dir(state: &Path, id: &SeatId) -> PathBuf {
    seat_dir(state, id).join("pin")
}

/// Copies the certificate the host in `home` serves into `dst`, root's alone, or removes the copy
/// while the host has none. `home` is a seat user's or the owner's, so it goes through
/// [`safe_open`].
pub(super) fn copy_pin(home: &Path, dst: &Path) -> std::io::Result<()> {
    const CERT: &str = "native-cert.pem";
    let config = safe_open::walk(safe_open::dir(home)?, &[".config", "punktfunk"])?;
    let found = match config {
        Some(at) => safe_open::read_at(at, CERT, 64 * 1024)?,
        None => None,
    };
    let Some((bytes, _)) = found else {
        return match std::fs::remove_file(dst.join(CERT)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    };
    let text = String::from_utf8(bytes).map_err(|_| std::io::ErrorKind::InvalidData)?;
    make_dir(dst, 0o700)?;
    super::session::write_if_changed(&dst.join(CERT), &text, 0o600).map(drop)
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

/// Copies `name`, at most `max` bytes, from `src` into `dst` for `reader` when it differs, and
/// removes it from `dst` when `src` has none. Returns whether `dst` changed. Both are directory
/// handles, so no link on either side is followed, whoever owns the directory. An unchanged file
/// is not read.
fn sync_file(
    src: Option<&OwnedFd>,
    dst: &OwnedFd,
    name: &str,
    reader: Reader,
    max: u64,
) -> std::io::Result<bool> {
    let found = match src {
        Some(src) => safe_open::file_at(src, name, max)?,
        None => None,
    };
    let Some((file, meta)) = found else {
        return unlink(dst, name);
    };
    let wanted = (meta.mtime(), meta.mtime_nsec(), meta.size());
    let current = rustix::fs::statat(dst, name, AtFlags::SYMLINK_NOFOLLOW)
        .ok()
        .map(|st| stamp(&st));
    if !needs_copy(wanted, current) {
        return Ok(false);
    }
    let bytes = safe_open::read_opened(file, &meta, name, max)?;
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
    let io = |what: &str, e| io_err("trust_copy", what, e);
    let dst = open_copy(&trust_dir(state, id), reader).map_err(|e| io("open the trust copy", e))?;
    let src = open_source(box_dir).map_err(|e| io("open the box directory", e))?;
    let mut changed = false;
    for name in RETIRED {
        changed |= unlink(&dst, name).map_err(|e| io(&format!("remove the old {name}"), e))?;
    }
    for name in TRUST_FILES {
        changed |= sync_file(src.as_ref(), &dst, name, reader, MAX_TRUST_BYTES)
            .map_err(|e| io(&format!("copy {name}"), e))?;
    }
    changed |= sync_flat_dir(src.as_ref(), &dst, AVATARS, reader, MAX_TRUST_BYTES)
        .map_err(|e| io("copy the avatars", e))?;
    Ok(changed)
}

/// Brings the box's library level with the owner's own in `home`, for `reader`, the door's
/// group. Returns whether anything changed. A config folder that isn't there, as in a home not
/// mounted yet, leaves the copy as it is: the door keeps serving the last library it had.
pub(super) fn refresh_box_library(
    home: &Path,
    state: &Path,
    reader: Reader,
) -> Result<bool, BackendError> {
    let io = |what: &str, e| io_err("box_library", what, e);
    let config = safe_open::dir(home).and_then(|at| safe_open::walk(at, &[".config", "punktfunk"]));
    let src = match config {
        Ok(Some(src)) => src,
        Ok(None) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io("open the owner's config folder", e)),
    };
    let dst =
        open_copy(&box_library_dir(state), reader).map_err(|e| io("open the library copy", e))?;
    sync_library(Some(&src), &dst, reader).map_err(|e| io("copy the owner's library", e))
}

/// Brings the box's library in a seat's trust copy level with [`box_library_dir`]: the seat's
/// `PUNKTFUNK_LIBRARY_DIR`. Never the owner's row, whose host writes the library it reads.
pub(super) fn refresh_seat_library(
    state: &Path,
    id: &SeatId,
    reader: Reader,
) -> Result<bool, BackendError> {
    let io = |what: &str, e| io_err("trust_copy", what, e);
    let dst = open_copy(&trust_dir(state, id), reader).map_err(|e| io("open the trust copy", e))?;
    let src = open_source(&box_library_dir(state)).map_err(|e| io("open the box library", e))?;
    sync_library(src.as_ref(), &dst, reader).map_err(|e| io("copy the box library", e))
}

/// Removes the box's library: the owner's row is gone, and so is the door it was for.
pub(super) fn remove_box_library(state: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(box_library_dir(state)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The library files and the metadata folder from `src` into `dst`, for `reader`.
fn sync_library(src: Option<&OwnedFd>, dst: &OwnedFd, reader: Reader) -> std::io::Result<bool> {
    let mut changed = false;
    for name in LIBRARY_FILES {
        changed |= sync_file(src, dst, name, reader, MAX_LIBRARY_BYTES)?;
    }
    changed |= sync_flat_dir(src, dst, LIBRARY_METADATA, reader, MAX_LIBRARY_BYTES)?;
    Ok(changed)
}

/// `dst` made and handed to `reader`, then opened: a copy root writes for one reader.
fn open_copy(dst: &Path, reader: Reader) -> std::io::Result<OwnedFd> {
    make_dir(dst, reader.dir_mode())?;
    std::os::unix::fs::chown(dst, reader.uid, Some(reader.gid))?;
    safe_open::dir(dst)
}

/// A directory a copy is taken from, `None` when it doesn't exist.
fn open_source(dir: &Path) -> std::io::Result<Option<OwnedFd>> {
    match safe_open::dir(dir) {
        Ok(src) => Ok(Some(src)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
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

/// Mirrors the flat directory `dir` of `src` into `dst`: copies each file of at most `max` bytes,
/// drops copies whose source is gone. A link where the directory should be is refused.
fn sync_flat_dir(
    src: Option<&OwnedFd>,
    dst: &OwnedFd,
    dir: &str,
    reader: Reader,
    max: u64,
) -> std::io::Result<bool> {
    let src = match src {
        Some(src) => safe_open::dir_at(src, dir)?,
        None => None,
    };
    if src.is_some() {
        let mode = Mode::from_raw_mode(reader.dir_mode() as _);
        match rustix::fs::mkdirat(dst, dir, mode) {
            Ok(()) => {}
            Err(e) if e == rustix::io::Errno::EXIST => {}
            Err(e) => return Err(e.into()),
        }
    }
    let Some(copies) = safe_open::dir_at(dst, dir)? else {
        return Ok(false);
    };
    let mut wanted = HashSet::new();
    let mut changed = false;
    if let Some(src) = &src {
        rustix::fs::fchmod(&copies, Mode::from_raw_mode(reader.dir_mode() as _))?;
        std::os::unix::fs::fchown(&copies, reader.uid, Some(reader.gid))?;
        for name in names(src)?.into_iter().filter(|n| safe_name(n)) {
            // A link or oversized entry is skipped, not copied and not fatal.
            if let Ok(did) = sync_file(Some(src), &copies, &name, reader, max) {
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
        let _ = rustix::fs::unlinkat(dst, dir, AtFlags::REMOVEDIR);
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

/// A Steam install to clone and the user it is read as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SteamSource {
    pub root: PathBuf,
    pub uid: u32,
    pub gid: u32,
}

impl SteamSource {
    /// The install at `root`, read as whoever owns it: `--steam-source`, which the operator names.
    pub(super) fn owned(root: PathBuf) -> Option<Self> {
        let meta = std::fs::metadata(&root).ok()?;
        Some(Self {
            root,
            uid: meta.uid(),
            gid: meta.gid(),
        })
    }
}

/// The Steam a new seat is cloned from: the lowest-uid ordinary user that has one, read as that
/// user. A box with several Steam users picks the first; `--steam-source` names another.
pub(super) fn find_steam_source(users: &[Passwd]) -> Option<SteamSource> {
    let mut ordinary: Vec<&Passwd> = users
        .iter()
        .filter(|u| (1000..60000).contains(&u.uid) && !u.gecos.starts_with("punktfunk-seat="))
        .collect();
    ordinary.sort_by_key(|u| u.uid);
    ordinary.into_iter().find_map(|u| {
        Some(SteamSource {
            root: steam_root_of(&u.home)?,
            uid: u.uid,
            gid: u.gid,
        })
    })
}

/// A Steam under the seat's home, cloned from `source` when there is one, and a library list that
/// names the shared games folder first. Never fails the seat: without a clone Steam downloads
/// itself on first run, and without the list it offers games again.
pub(super) fn provision_steam(
    state: &Path,
    id: &SeatId,
    passwd: &Passwd,
    source: Option<&SteamSource>,
) {
    let steam = passwd.home.join(".local/share/Steam");
    if !passwd.home.join(STEAM_MARKER).exists() {
        match source {
            Some(src) => {
                let stage = seat_dir(state, id).join("steam-clone");
                if let Err(error) = clone_install(src, &stage, &steam, passwd) {
                    let _ = std::fs::remove_dir_all(&stage);
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

/// `cp -a --reflink=auto` of every non-skipped top-level entry, run as the source's user: root
/// reading a user's tree would follow what that user links. The copy lands in `stage`, which only
/// root and that user reach, then goes to the seat's user and moves to `dst`. A filesystem without
/// reflinks makes a real copy, which takes a while but leaves the seat a working Steam.
fn clone_install(
    src: &SteamSource,
    stage: &Path,
    dst: &Path,
    seat: &Passwd,
) -> Result<(), BackendError> {
    let io = |what: &str, e| io_err("steam_clone", what, e);
    let names = clone_names(src).map_err(|e| io("read steam", e))?;
    if names.is_empty() {
        return Err(err(
            "steam_clone",
            "the steam install holds nothing to clone",
        ));
    }
    let _ = std::fs::remove_dir_all(stage);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(stage)
        .map_err(|e| io("create the stage", e))?;
    std::os::unix::fs::chown(stage, Some(src.uid), Some(src.gid))
        .map_err(|e| io("own the stage", e))?;
    let (uid, gid) = (src.uid.to_string(), src.gid.to_string());
    let mut args = vec![
        "--reuid".to_owned(),
        uid,
        "--regid".to_owned(),
        gid,
        "--clear-groups".to_owned(),
        "cp".to_owned(),
        "-a".to_owned(),
        "--reflink=auto".to_owned(),
        "--".to_owned(),
    ];
    args.extend(
        names
            .iter()
            .map(|name| src.root.join(name).to_string_lossy().into_owned()),
    );
    args.push(stage.to_string_lossy().into_owned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run("steam_clone", "setpriv", &args)?;
    let owner = format!("{}:{}", seat.uid, seat.gid);
    run(
        "chown_failed",
        "chown",
        &["-R", &owner, &stage.to_string_lossy()],
    )?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io("create the seat steam", e))?;
    }
    let _ = std::fs::remove_dir_all(dst);
    std::fs::rename(stage, dst).map_err(|e| io("move the copy into place", e))
}

/// The top-level entries of `src` a seat inherits. Its folder must be its user's own: one it
/// links elsewhere would have root list a directory that user can't.
fn clone_names(src: &SteamSource) -> std::io::Result<Vec<String>> {
    let dir = safe_open::dir(&src.root)?;
    if rustix::fs::fstat(&dir)?.st_uid != src.uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} isn't its user's own", src.root.display()),
        ));
    }
    let mut out: Vec<String> = names(&dir)?
        .into_iter()
        .filter(|name| !excluded_from_clone(name))
        .collect();
    out.sort();
    Ok(out)
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

    /// An access ACL: owner `rw`, group `r`, a named group `rw`, the mask, other `r`.
    fn acl(mask: u16) -> Vec<u8> {
        let entry = |tag: u16, perm: u16, id: u32| {
            [
                &tag.to_le_bytes()[..],
                &perm.to_le_bytes(),
                &id.to_le_bytes(),
            ]
            .concat()
        };
        let none = u32::MAX;
        [
            2u32.to_le_bytes().to_vec(),
            entry(0x01, 6, none),
            entry(0x04, 4, none),
            entry(0x08, 6, 4242),
            entry(ACL_MASK, mask, none),
            entry(0x20, 4, none),
        ]
        .concat()
    }

    /// `m::rwX` as `setfacl` reads it, and nothing done to an ACL it can't read.
    #[test]
    fn the_acl_mask_becomes_rw_with_x_where_asked() {
        assert_eq!(with_mask(&acl(4), 6), Some(acl(6)));
        assert_eq!(with_mask(&acl(5), 7), Some(acl(7)));
        assert_eq!(with_mask(&acl(6), 6), None, "already rw");
        assert_eq!(with_mask(&acl(4)[..20], 6), None, "torn");
        let mut version_1 = acl(4);
        version_1[0] = 1;
        assert_eq!(with_mask(&version_1, 6), None, "not version 2");
    }

    /// The games walk repairs what is inside and never what a link leads to, also when a folder
    /// it listed is swapped for a link before it opens it. POSIX ACLs are Linux's.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_link_swapped_in_mid_walk_is_not_followed() {
        let mask_of = |path: &Path| {
            let mut buf = [0u8; 1024];
            let len = rustix::fs::getxattr(path, ACL_ACCESS, &mut buf[..]).unwrap();
            let mut entries = buf[4..len].chunks_exact(8);
            let mask = entries.find(|e| u16::from_le_bytes([e[0], e[1]]) == ACL_MASK);
            mask.map(|e| u16::from_le_bytes([e[2], e[3]])).unwrap()
        };
        let temp = tempfile::tempdir().unwrap();
        let (games, outside) = (temp.path().join("games"), temp.path().join("outside"));
        std::fs::create_dir_all(games.join("sub")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(games.join("sub/a"), "x").unwrap();
        std::fs::write(outside.join("b"), "x").unwrap();
        for path in [games.join("sub"), games.join("sub/a"), outside.join("b")] {
            let set = rustix::fs::setxattr(&path, ACL_ACCESS, &acl(4), XattrFlags::empty());
            if set.is_err() {
                eprintln!("no POSIX ACLs under {}: skipped", temp.path().display());
                return;
            }
        }
        rustix::fs::setxattr(&outside, ACL_ACCESS, &acl(4), XattrFlags::empty()).unwrap();
        let handle = safe_open::dir(&games).unwrap();
        let mut listing = rustix::fs::Dir::read_from(&handle).unwrap();
        let listed = std::iter::from_fn(|| listing.read())
            .map(Result::unwrap)
            .find(|e| e.file_name() == c"sub")
            .unwrap();
        assert_eq!(listed.file_type(), rustix::fs::FileType::Directory);
        std::fs::rename(games.join("sub"), games.join("sub.old")).unwrap();
        std::os::unix::fs::symlink(&outside, games.join("sub")).unwrap();
        let opened = mask_entry(handle.as_fd(), listed.file_name()).unwrap();
        assert!(opened.is_none(), "the link is not opened");
        mask_tree(safe_open::dir(&games).unwrap()).unwrap();
        assert_eq!(mask_of(&outside), 4, "the link's target");
        assert_eq!(mask_of(&outside.join("b")), 4);
        assert_eq!(mask_of(&games.join("sub.old")), 7, "a folder takes x");
        assert_eq!(mask_of(&games.join("sub.old/a")), 6);
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

    /// The box's library follows the owner's own files and reaches a seat's trust copy, minus
    /// play stats and anything else in the owner's config folder.
    #[test]
    fn the_box_library_follows_the_owner_into_each_seat() {
        let temp = tempfile::tempdir().unwrap();
        let (home, state) = (temp.path().join("home"), temp.path().join("state"));
        let config = home.join(".config/punktfunk");
        std::fs::create_dir_all(config.join("library-metadata")).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::write(config.join("library.json"), r#"{"entries":[]}"#).unwrap();
        std::fs::write(config.join("library-metadata/sgdb.json"), "{}").unwrap();
        std::fs::write(config.join("library-stats.json"), "{}").unwrap();
        std::fs::write(config.join("mgmt-token"), "secret").unwrap();
        let reader = Reader::group(own_gid(&home));

        assert!(refresh_box_library(&home, &state, reader).unwrap());
        let copy = box_library_dir(&state);
        assert_eq!(mode(&copy), 0o750);
        assert_eq!(mode(&copy.join("library.json")), 0o640);
        assert!(copy.join("library-metadata/sgdb.json").exists());
        for kept in ["library-stats.json", "mgmt-token"] {
            assert!(!copy.join(kept).exists(), "{kept} stays the owner's");
        }
        assert!(
            !refresh_box_library(&home, &state, reader).unwrap(),
            "unchanged is a no-op"
        );

        assert!(refresh_seat_library(&state, &id(), reader).unwrap());
        let trust = trust_dir(&state, &id());
        assert!(trust.join("library.json").exists());
        assert!(trust.join("library-metadata/sgdb.json").exists());

        // A file the owner drops leaves both copies.
        std::fs::remove_file(config.join("library-metadata/sgdb.json")).unwrap();
        assert!(refresh_box_library(&home, &state, reader).unwrap());
        assert!(refresh_seat_library(&state, &id(), reader).unwrap());
        assert!(!trust.join("library-metadata/sgdb.json").exists());

        remove_box_library(&state).unwrap();
        assert!(!copy.exists());
        assert!(refresh_seat_library(&state, &id(), reader).unwrap());
        assert!(!trust.join("library.json").exists());
    }

    /// An owner's config folder that isn't there, as in a home not mounted yet, keeps the copy
    /// the door serves; a link among the owner's files is refused and not followed.
    #[test]
    fn the_box_library_survives_a_missing_home_and_refuses_a_link() {
        let temp = tempfile::tempdir().unwrap();
        let (home, state) = (temp.path().join("home"), temp.path().join("state"));
        let config = home.join(".config/punktfunk");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::write(config.join("library.json"), "{}").unwrap();
        let reader = Reader::group(own_gid(&home));
        assert!(refresh_box_library(&home, &state, reader).unwrap());

        let gone = temp.path().join("gone");
        assert!(!refresh_box_library(&gone, &state, reader).unwrap());
        assert!(box_library_dir(&state).join("library.json").exists());

        let secret = temp.path().join("secret");
        std::fs::write(&secret, "root only").unwrap();
        std::fs::remove_file(config.join("library.json")).unwrap();
        std::os::unix::fs::symlink(&secret, config.join("library.json")).unwrap();
        let error = refresh_box_library(&home, &state, reader).unwrap_err();
        assert_eq!(error.code, "box_library");
        assert_ne!(
            std::fs::read_to_string(box_library_dir(&state).join("library.json")).unwrap(),
            "root only"
        );
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
            Some(SteamSource {
                root: temp.path().join("early/.local/share/Steam"),
                uid: 1001,
                gid: 1001,
            })
        );
        assert_eq!(find_steam_source(&users[..2]), None);
    }

    /// A link where the box's avatar folder should be is refused, and nothing behind it is
    /// copied into a seat's trust copy.
    #[test]
    fn a_linked_profiles_directory_is_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let (box_dir, state) = (temp.path().join("box"), temp.path().join("state"));
        let secret = temp.path().join("secret");
        for dir in [&box_dir, &state, &secret] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(secret.join("shadow.png"), "root only").unwrap();
        std::os::unix::fs::symlink(&secret, box_dir.join("profiles")).unwrap();
        let gid = own_gid(&box_dir);
        let error = refresh_trust(&box_dir, &state, &id(), Reader::group(gid)).unwrap_err();
        assert_eq!(error.code, "trust_copy");
        assert!(!trust_dir(&state, &id())
            .join("profiles/shadow.png")
            .exists());
    }

    /// A FIFO the door puts among its files doesn't stall the supervisor: one in the avatar
    /// folder is skipped, one at a file the copy reads fails the refresh at once.
    #[test]
    fn a_fifo_in_the_box_does_not_block_the_copy() {
        use super::super::safe_open::tests::{fifo, within};
        let temp = tempfile::tempdir().unwrap();
        let (box_dir, state) = (temp.path().join("box"), temp.path().join("state"));
        std::fs::create_dir_all(box_dir.join("profiles")).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::write(box_dir.join("profiles.json"), "{}").unwrap();
        fifo(&box_dir.join("profiles/p1.png"));
        let gid = own_gid(&box_dir);
        let refresh = || {
            let (b, s) = (box_dir.clone(), state.clone());
            within(move || refresh_trust(&b, &s, &id(), Reader::group(gid)).is_ok())
        };
        assert_eq!(refresh(), Some(true), "an avatar FIFO is skipped");
        fifo(&box_dir.join("seat-defaults.json"));
        assert_eq!(refresh(), Some(false), "a box file FIFO fails the refresh");
    }

    /// A FIFO for the seat host's certificate answers at once and leaves no copy; a real one is
    /// copied for root alone.
    #[test]
    fn a_fifo_for_the_host_certificate_does_not_block() {
        use super::super::safe_open::tests::{fifo, within};
        let temp = tempfile::tempdir().unwrap();
        let (home, pin) = (temp.path().join("home"), temp.path().join("pin"));
        let config = home.join(".config/punktfunk");
        std::fs::create_dir_all(&config).unwrap();
        fifo(&config.join("native-cert.pem"));
        let (h, p) = (home.clone(), pin.clone());
        let copied = within(move || copy_pin(&h, &p).is_ok()).expect("blocked on the FIFO");
        assert!(!copied);
        assert!(!pin.join("native-cert.pem").exists());
        std::fs::remove_file(config.join("native-cert.pem")).unwrap();
        std::fs::write(config.join("native-cert.pem"), "pem").unwrap();
        copy_pin(&home, &pin).unwrap();
        assert_eq!(
            std::fs::read_to_string(pin.join("native-cert.pem")).unwrap(),
            "pem"
        );
        assert_eq!(mode(&pin.join("native-cert.pem")), 0o600);
    }

    /// The supervisor refuses a ledger root another uid owns or may write. Giving a directory
    /// away needs root, so elsewhere this checks nothing.
    #[test]
    fn a_ledger_root_with_another_owner_is_refused() {
        if !rustix::process::geteuid().is_root() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = ledger_root(temp.path());
        ensure_layout(temp.path()).unwrap();
        std::os::unix::fs::chown(&root, Some(1000), None).unwrap();
        assert!(ensure_layout(temp.path()).is_err(), "owned by another uid");
        std::os::unix::fs::chown(&root, Some(0), None).unwrap();
        ensure_layout(temp.path()).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(ensure_layout(temp.path()).is_err(), "writable by others");
    }
}
