//! The box owner's Steam libraries, shown to a seat in place.
//!
//! Every library the owner's Steam lists appears in the seat at `seats/<id>/libraries/<key>`.
//! Its `steamapps` arrives through an idmapped mount: on disk each file stays the owner's, in the
//! seat it is the seat user's, so the seat's Steam plays, updates and runs Proton from it with no
//! ACL. The seat's own `compatdata`, `shadercache`, `downloading` and `temp` under
//! `seats/<id>/private/<key>` cover the owner's, and the library's own path is inaccessible to the
//! unit, so the owner's prefixes never reach a seat.
//!
//! The supervisor holds no `CAP_SYS_ADMIN`, so the seat unit's `ExecStartPre=+` mounts each
//! library at a root-only stage under [`STAGE_ROOT`], through `nsenter` into PID 1's namespace:
//! systemd gives even a `+` command a namespace of its own once the unit has one. `BindPaths=`
//! then carries the stage, idmap and all, into the seat. The libraries are read at each start; a
//! drive the owner mounts later reaches the seat on its next start.
//!
//! The box's `seat_library_writable` off (`library_writable` in [`DEFAULTS`]) mounts them
//! read-only: a seat plays what is installed, and can't update, install or rewrite an
//! appmanifest there. The few files a game needs to write get private copies ([`spots`]).

use super::accounts::Passwd;
use super::shared::{self, make_dir};
use super::{err, run};
use crate::backend::BackendError;
use crate::model::SeatId;
use crate::steam;
use std::collections::HashSet;
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// What a seat keeps its own of in each library. Steam stages updates in `temp`.
pub(super) const PRIVATE: [&str; 4] = ["compatdata", "shadercache", "downloading", "temp"];

/// Root-only: each seat's idmapped mounts wait here for its unit to bind them.
const STAGE_ROOT: &str = "/run/punktfunk/libraries";

/// Filesystems that take an idmapped mount. A library elsewhere (exFAT, NTFS, FUSE) is skipped.
const IDMAP_FS: [&str; 4] = ["btrfs", "ext4", "xfs", "f2fs"];

/// `pf_host_config::seat_defaults::FILE`, which the box writes into its config dir.
const DEFAULTS: &str = "seat-defaults.json";

const LIST: &str = "libraryfolders.vdf";
const LIST_TEMP: &str = "libraryfolders.vdf.punktfunk";
const MAX_LIST_BYTES: u64 = 8 * 1024 * 1024;

/// One of the owner's libraries a seat sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Library {
    /// The owner's `steamapps`, canonical.
    pub steamapps: PathBuf,
    /// Its folder under the seat's: a hash of the path, so the seat's prefixes stay with it.
    pub key: String,
    /// What the seat writes in it while it is read-only; empty while it is writable.
    pub spots: Vec<Spot>,
}

/// A path under a read-only `steamapps` the seat gets a private copy of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Spot {
    /// Relative to `steamapps`.
    pub path: PathBuf,
    pub dir: bool,
}

/// What a seat's unit needs to show the owner's libraries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Shares {
    pub nsenter: PathBuf,
    pub mount: PathBuf,
    pub umount: PathBuf,
    /// `X-mount.idmap=`: the owner's ids on disk, the seat's in the mount.
    pub idmap: String,
    /// The box's `seat_library_writable` is off.
    pub read_only: bool,
    pub libraries: Vec<Library>,
}

fn libraries_dir(box_dir: &Path, id: &SeatId) -> PathBuf {
    shared::seat_dir(box_dir, id).join("libraries")
}

/// The library as the seat's Steam lists it.
fn root_dir(box_dir: &Path, id: &SeatId, key: &str) -> PathBuf {
    libraries_dir(box_dir, id).join(key)
}

fn private_dir(box_dir: &Path, id: &SeatId, key: &str) -> PathBuf {
    shared::seat_dir(box_dir, id).join("private").join(key)
}

fn stage_dir(id: &SeatId, key: &str) -> PathBuf {
    Path::new(STAGE_ROOT).join(id.as_str()).join(key)
}

fn key(steamapps: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let bytes = steamapps.as_os_str().as_bytes();
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
    hex::encode(&digest.as_ref()[..8])
}

/// libmount writes each triple into the mount's user namespace as given, and the kernel reads the
/// first id as the one on disk. mount(8) names the two columns the other way around.
fn idmap(owner: (u32, u32), seat: &Passwd) -> String {
    format!("u:{}:{}:1 g:{}:{}:1", owner.0, seat.uid, owner.1, seat.gid)
}

/// A path a unit file carries inside double quotes as written: no escape, specifier or variable,
/// and no `:`, which `BindPaths=` reads as the source/target split.
fn unit_safe(path: &str) -> bool {
    !path
        .chars()
        .any(|c| c.is_control() || matches!(c, '"' | '\\' | '%' | '$' | ':'))
}

/// The library at `path` as a seat would see it, with its `steamapps`' device and inode, or why
/// a seat can't.
fn check(
    path: &str,
    owner_uid: u32,
    fstype: &impl Fn(&Path) -> Option<String>,
) -> Result<(Library, (u64, u64)), &'static str> {
    let canonical = std::fs::canonicalize(path).map_err(|_| "not found")?;
    let steamapps = canonical.join("steamapps");
    if !steamapps.to_str().is_some_and(unit_safe) {
        return Err("its path has characters a unit file can't carry");
    }
    let meta = std::fs::symlink_metadata(&steamapps).map_err(|_| "no steamapps folder")?;
    if !meta.is_dir() {
        return Err("no steamapps folder");
    }
    if meta.uid() != owner_uid {
        return Err("its steamapps isn't the owner's");
    }
    if !fstype(&steamapps).is_some_and(|t| IDMAP_FS.contains(&t.as_str())) {
        return Err("its filesystem can't take an idmapped mount");
    }
    for name in PRIVATE {
        let private = std::fs::symlink_metadata(steamapps.join(name));
        if private.is_ok_and(|m| !m.is_dir()) {
            return Err("a folder a seat keeps its own of is a link or a file");
        }
    }
    let inode = (meta.dev(), meta.ino());
    Ok((
        Library {
            key: key(&steamapps),
            steamapps,
            spots: Vec::new(),
        },
        inode,
    ))
}

/// The libraries at `paths` a seat can see, each once.
fn find(
    paths: &[String],
    owner_uid: u32,
    fstype: impl Fn(&Path) -> Option<String>,
) -> Vec<Library> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        match check(path, owner_uid, &fstype) {
            Ok((library, inode)) => {
                if seen.insert(inode) {
                    out.push(library);
                }
            }
            Err(reason) => {
                tracing::info!(library = %path, reason, "owner's library not shown to seats")
            }
        }
    }
    out
}

/// The folders in `dir` a unit file can name, sorted. A link is not a folder.
fn folders(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| unit_safe(name))
        .collect();
    out.sort();
    out
}

/// What a game still writes in a read-only `steamapps`, as found there: each Steam Linux
/// Runtime's `var` and each of its platforms' `files/.ref`, which pressure-vessel opens before a
/// native game starts, and each Proton's `dist.lock`. Read-only, either fails the launch with
/// EROFS. Links are never followed; a file that isn't there yet stays read-only.
fn spots(steamapps: &Path) -> Vec<Spot> {
    let found = |spot: &Spot| {
        let meta = std::fs::symlink_metadata(steamapps.join(&spot.path));
        meta.is_ok_and(|m| if spot.dir { m.is_dir() } else { m.is_file() })
    };
    let spot = |path: PathBuf, dir| Spot { path, dir };
    let mut out = Vec::new();
    for name in folders(&steamapps.join("common")) {
        let tool = Path::new("common").join(&name);
        if name.starts_with("SteamLinuxRuntime_") {
            out.push(spot(tool.join("var"), true));
            for platform in folders(&steamapps.join(&tool)) {
                let files = spot(tool.join(platform).join("files"), true);
                if found(&files) {
                    out.push(spot(files.path.join(".ref"), false));
                }
            }
        } else if name.starts_with("Proton") {
            out.push(spot(tool.join("dist.lock"), false));
        }
    }
    out.retain(found);
    out
}

/// The box's `seat_library_writable`, read from its [`DEFAULTS`] in `box_dir`: on when the box
/// wrote none or the file is unreadable.
fn writable(box_dir: &Path) -> bool {
    let read = || -> std::io::Result<Option<bool>> {
        let Some((file, _)) = shared::open_regular(&box_dir.join(DEFAULTS))? else {
            return Ok(None);
        };
        let defaults: serde_json::Value = serde_json::from_reader(file)?;
        Ok(defaults["library_writable"].as_bool())
    };
    read()
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "seat defaults unreadable; the owner's libraries stay writable");
            None
        })
        .unwrap_or(true)
}

fn fstype(path: &Path) -> Option<String> {
    let path = path.to_str()?;
    let found = run(
        "findmnt",
        "findmnt",
        &["-n", "-o", "FSTYPE", "--target", path],
    );
    Some(found.ok()?.trim().to_owned())
}

/// The libraries of the owner's Steam at `steam_root` for seat `seat`, their folders in place,
/// read-only when the box says so. `None` when there is nothing to show: no such Steam, one that
/// is root's or the seat's, or no list. Root's is never mapped: a seat would own every root file
/// in the library.
pub(super) fn share(
    box_dir: &Path,
    id: &SeatId,
    seat: &Passwd,
    steam_root: Option<&Path>,
) -> Option<Shares> {
    let steam_root = steam_root?;
    let meta = std::fs::metadata(steam_root).ok()?;
    let owner = (meta.uid(), meta.gid());
    if owner.0 == 0 || owner.1 == 0 || owner.0 == seat.uid {
        let steam = steam_root.display();
        tracing::info!(%steam, "steam is root's or the seat's; no library shown");
        return None;
    }
    let [Some(nsenter), Some(mount), Some(umount)] =
        ["nsenter", "mount", "umount"].map(super::which)
    else {
        tracing::warn!("nsenter, mount or umount not found; no library shown");
        return None;
    };
    let list = steam_root.join("steamapps").join(LIST);
    let vdf = match shared::open_regular(&list) {
        Ok(Some((mut file, _))) => {
            let mut text = String::new();
            file.read_to_string(&mut text).ok()?;
            text
        }
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(%error, "owner's steam library list unreadable");
            return None;
        }
    };
    let read_only = !writable(box_dir);
    let libraries = find(&steam::library_paths(&vdf), owner.0, fstype)
        .into_iter()
        .map(|mut library| {
            if read_only {
                library.spots = spots(&library.steamapps);
            }
            library
        })
        .filter(|library| match prepare(box_dir, id, seat, owner, library) {
            Ok(()) => true,
            Err(error) => {
                let library = library.steamapps.display();
                tracing::warn!(%library, %error, "owner's library not shown to the seat");
                false
            }
        })
        .collect();
    Some(Shares {
        nsenter,
        mount,
        umount,
        idmap: idmap(owner, seat),
        read_only,
        libraries,
    })
}

/// Every directory a library's binds name, and the seat's copy of each of its [`Spot`]s, before
/// the unit starts: a missing one fails it with `226/NAMESPACE`, and none can be made inside the
/// idmapped mount, where root has no id.
fn prepare(
    box_dir: &Path,
    id: &SeatId,
    seat: &Passwd,
    owner: (u32, u32),
    library: &Library,
) -> Result<(), BackendError> {
    let io = |what: &str, e: std::io::Error| err("library_dirs", format!("{what}: {e}"));
    let missing: Vec<String> = PRIVATE
        .iter()
        .map(|name| library.steamapps.join(name))
        .filter(|path| !path.exists())
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    if !missing.is_empty() {
        // As the owner: root walking the owner's tree would follow what the owner links.
        let (uid, gid) = (owner.0.to_string(), owner.1.to_string());
        let mut args = vec![
            "--reuid",
            &uid,
            "--regid",
            &gid,
            "--clear-groups",
            "mkdir",
            "--",
        ];
        args.extend(missing.iter().map(String::as_str));
        run("library_dirs", "setpriv", &args)?;
    }
    make_dir(&libraries_dir(box_dir, id), 0o711).map_err(|e| io("create the libraries", e))?;
    let root = root_dir(box_dir, id, &library.key);
    let mount_point = root.join("steamapps");
    std::fs::create_dir_all(&root).map_err(|e| io("create the library", e))?;
    // The mount point is made while the folder is root's alone; the sticky bit then keeps the
    // seat, which writes beside it, from swapping it for a link.
    match std::fs::symlink_metadata(&mount_point) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(err(
                "library_dirs",
                "the library's mount point isn't a folder",
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| io("close the library", e))?;
            std::fs::create_dir(&mount_point).map_err(|e| io("create the mount point", e))?;
        }
        Err(e) => return Err(io("read the mount point", e)),
    }
    std::os::unix::fs::chown(&root, Some(0), Some(seat.gid))
        .map_err(|e| io("own the library", e))?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o1770))
        .map_err(|e| io("open the library to the seat", e))?;
    let private = private_dir(box_dir, id, &library.key);
    make_dir(&shared::seat_dir(box_dir, id).join("private"), 0o711)
        .and_then(|()| make_dir(&private, 0o711))
        .map_err(|e| io("create the private folders", e))?;
    for name in PRIVATE {
        let dir = private.join(name);
        make_dir(&dir, 0o700).map_err(|e| io("create a private folder", e))?;
        std::os::unix::fs::chown(&dir, Some(seat.uid), Some(seat.gid))
            .map_err(|e| io("own a private folder", e))?;
    }
    make_spots(&private, &library.spots, seat).map_err(|e| io("create a private copy", e))?;
    make_dir(Path::new(STAGE_ROOT), 0o700).map_err(|e| io("create the stage", e))?;
    std::fs::create_dir_all(stage_dir(id, &library.key)).map_err(|e| io("create the stage", e))
}

/// The seat's own copy of each spot under `private`, empty and the seat user's. Root makes the
/// folders on the way; a copy already there is kept.
fn make_spots(private: &Path, spots: &[Spot], seat: &Passwd) -> std::io::Result<()> {
    for spot in spots {
        let path = private.join(&spot.path);
        if spot.dir {
            make_dir(&path, 0o700)?;
            std::os::unix::fs::chown(&path, Some(seat.uid), Some(seat.gid))?;
        } else {
            std::fs::create_dir_all(path.parent().unwrap_or(private))?;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                .mode(0o644)
                .open(&path)?;
            std::os::unix::fs::fchown(&file, Some(seat.uid), Some(seat.gid))?;
        }
    }
    Ok(())
}

/// The drop-in lines that show `shares` to seat `id`: mount each library at its stage before the
/// unit starts, read-only if the box says so, and unmount it after; hide its own path, bind the
/// stage, the seat's private folders and its spots' copies over it, and keep the owner's library
/// list there read-only. A mount that fails leaves the seat an empty library, not a failed start.
pub(super) fn unit_lines(box_dir: &Path, id: &SeatId, shares: &Shares) -> String {
    let host = format!("{} -t 1 -m --", shares.nsenter.display());
    let ro = if shares.read_only { "ro," } else { "" };
    let mut out = String::new();
    for library in &shares.libraries {
        let stage = stage_dir(id, &library.key);
        let target = root_dir(box_dir, id, &library.key).join("steamapps");
        let private = private_dir(box_dir, id, &library.key);
        let (stage, owner) = (stage.display(), library.steamapps.display());
        out.push_str(&format!(
            "ExecStartPre=-+{host} {} --bind -o \"{ro}nosuid,nodev,X-mount.idmap={}\" \"{owner}\" {stage}\n\
             ExecStopPost=-+{host} {} -l {stage}\n\
             InaccessiblePaths=\"-{owner}\"\n\
             BindPaths={stage}:{}\n",
            shares.mount.display(),
            shares.idmap,
            shares.umount.display(),
            target.display(),
        ));
        for name in PRIVATE {
            out.push_str(&format!(
                "BindPaths={}:{}\n",
                private.join(name).display(),
                target.join(name).display()
            ));
        }
        // Each side quoted on its own: a spot's path may hold a space.
        for spot in &library.spots {
            out.push_str(&format!(
                "BindPaths=\"{}\":\"{}\"\n",
                private.join(&spot.path).display(),
                target.join(&spot.path).display()
            ));
        }
        let list = target.join(LIST);
        out.push_str(&format!(
            "BindReadOnlyPaths=-{stage}/{LIST}:{}\n",
            list.display()
        ));
    }
    out
}

/// Lists `libraries` in the seat's Steam and drops the ones it no longer has, and any folder
/// under `/run/media`, which a seat never sees. Steam's own entries stay as written. Never fails
/// the start: an unlisted library is one the seat's Steam doesn't offer.
pub(super) fn register(box_dir: &Path, id: &SeatId, seat: &Passwd, libraries: &[Library]) {
    let ours = libraries_dir(box_dir, id);
    let wanted: Vec<String> = libraries
        .iter()
        .map(|l| root_dir(box_dir, id, &l.key).to_string_lossy().into_owned())
        .collect();
    let managed =
        |path: &str| Path::new(path).starts_with(&ours) || path.starts_with("/run/media/");
    // Steam writes both copies and reads the one under `config`.
    for dir in ["steamapps", "config"] {
        let merge = |vdf: &str| steam::merge_library_folders(vdf, managed, &wanted);
        if let Err(error) = merge_list(&seat.home, dir, seat, merge) {
            tracing::warn!(%error, dir, "seat's steam library list not updated");
        }
    }
}

/// Rewrites `.local/share/Steam/<dir>/libraryfolders.vdf` under `home` through `merge`. Root
/// writes in a tree the seat user owns, so no link on the way is followed, and the new file is
/// the seat's. An absent file stays absent: Steam writes its own on first run.
fn merge_list(
    home: &Path,
    dir: &str,
    seat: &Passwd,
    merge: impl Fn(&str) -> Option<String>,
) -> std::io::Result<()> {
    use rustix::fs::{Mode, OFlags};
    let open = |at: &OwnedFd, name: &str, flags: OFlags| {
        let flags = flags | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match rustix::fs::openat(at, name, flags, Mode::empty()) {
            Ok(fd) => Ok(Some(fd)),
            Err(e) if e == rustix::io::Errno::NOENT => Ok(None),
            Err(e) => Err(std::io::Error::from(e)),
        }
    };
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut at = rustix::fs::open(home, flags, Mode::empty())?;
    for name in [".local", "share", "Steam", dir] {
        match open(&at, name, OFlags::DIRECTORY)? {
            Some(next) => at = next,
            None => return Ok(()),
        }
    }
    match open(&at, LIST, OFlags::empty())? {
        Some(file) => rewrite(&at, std::fs::File::from(file), seat, merge),
        None => Ok(()),
    }
}

fn rewrite(
    dir: &OwnedFd,
    mut file: std::fs::File,
    seat: &Passwd,
    merge: impl Fn(&str) -> Option<String>,
) -> std::io::Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    let invalid =
        |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned());
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() > 1 || meta.len() > MAX_LIST_BYTES {
        return Err(invalid("not a plain single-link file within the size cap"));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let merged = merge(&text).ok_or_else(|| invalid("not a steam library list"))?;
    if merged == text {
        return Ok(());
    }
    let _ = rustix::fs::unlinkat(dir, LIST_TEMP, AtFlags::empty());
    let create =
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let temp = rustix::fs::openat(dir, LIST_TEMP, create, Mode::RUSR | Mode::WUSR)?;
    let mut temp = std::fs::File::from(temp);
    temp.write_all(merged.as_bytes())?;
    temp.set_permissions(std::fs::Permissions::from_mode(meta.mode() & 0o777))?;
    std::os::unix::fs::fchown(&temp, Some(seat.uid), Some(seat.gid))?;
    drop(temp);
    rustix::fs::renameat(dir, LIST_TEMP, dir, LIST)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> SeatId {
        SeatId::parse("0123456789abcdef0123456789abcdef").unwrap()
    }

    /// A seat user that is whoever owns `home`, so the test can hand files to it.
    fn me(home: &Path) -> Passwd {
        let meta = std::fs::metadata(home).unwrap();
        Passwd {
            name: "pf-seat-1".into(),
            uid: meta.uid(),
            gid: meta.gid(),
            gecos: String::new(),
            home: home.to_path_buf(),
        }
    }

    fn btrfs(_: &Path) -> Option<String> {
        Some("btrfs".into())
    }

    #[test]
    fn the_idmap_puts_the_owner_s_ids_on_disk_first() {
        let seat = Passwd {
            uid: 987,
            gid: 956,
            ..me(Path::new("/"))
        };
        assert_eq!(idmap((1000, 1000), &seat), "u:1000:987:1 g:1000:956:1");
    }

    #[test]
    fn a_library_s_key_follows_its_path() {
        let a = key(Path::new("/run/media/bazzite/QEMU HARDDISK/steamapps"));
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(
            a,
            key(Path::new("/run/media/bazzite/QEMU HARDDISK/steamapps"))
        );
        assert_ne!(
            a,
            key(Path::new("/run/media/bazzite/QEMU HARDDISK1/steamapps"))
        );
    }

    /// Only a library a seat can use is shown, and each reason to skip one is named.
    #[test]
    fn a_library_is_shown_only_when_a_seat_can_use_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let uid = std::fs::metadata(&root).unwrap().uid();
        let library = |name: &str| {
            let dir = root.join(name);
            std::fs::create_dir_all(dir.join("steamapps/compatdata")).unwrap();
            dir.to_string_lossy().into_owned()
        };
        let reason = |path: &str, owner: u32, fs: &str| {
            let fs = fs.to_owned();
            check(path, owner, &|_: &Path| Some(fs.clone())).map(|(l, _)| l)
        };

        let good = library("QEMU HARDDISK");
        let shown = reason(&good, uid, "btrfs").unwrap();
        assert_eq!(shown.steamapps, root.join("QEMU HARDDISK/steamapps"));
        assert_eq!(reason(&good, uid, "ext4"), Ok(shown.clone()));

        let missing = root.join("QEMU HARDDISK1").to_string_lossy().into_owned();
        assert_eq!(reason(&missing, uid, "btrfs"), Err("not found"));
        assert_eq!(
            reason(&good, uid + 1, "btrfs"),
            Err("its steamapps isn't the owner's")
        );
        for fs in ["exfat", "ntfs3", "fuseblk", "vfat"] {
            assert_eq!(
                reason(&good, uid, fs),
                Err("its filesystem can't take an idmapped mount"),
                "{fs}"
            );
        }

        let empty = root.join("empty");
        std::fs::create_dir(&empty).unwrap();
        let empty = empty.to_string_lossy().into_owned();
        assert_eq!(reason(&empty, uid, "btrfs"), Err("no steamapps folder"));
        let linked = root.join("linked");
        std::fs::create_dir(&linked).unwrap();
        let target = root.join("QEMU HARDDISK/steamapps");
        std::os::unix::fs::symlink(target, linked.join("steamapps")).unwrap();
        let linked = linked.to_string_lossy().into_owned();
        assert_eq!(reason(&linked, uid, "btrfs"), Err("no steamapps folder"));

        let quoted = library("say \"hi\"");
        assert_eq!(
            reason(&quoted, uid, "btrfs"),
            Err("its path has characters a unit file can't carry")
        );
        let split = library("a:b");
        assert_eq!(
            reason(&split, uid, "btrfs"),
            Err("its path has characters a unit file can't carry"),
            "a `:` would split a BindPaths= line"
        );
        let prefixes_elsewhere = library("prefixes elsewhere");
        let compatdata = root.join("prefixes elsewhere/steamapps/compatdata");
        std::fs::remove_dir(&compatdata).unwrap();
        std::os::unix::fs::symlink(&root, &compatdata).unwrap();
        assert_eq!(
            reason(&prefixes_elsewhere, uid, "btrfs"),
            Err("a folder a seat keeps its own of is a link or a file")
        );
    }

    #[test]
    fn a_library_listed_twice_is_shown_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::create_dir_all(root.join("drive/steamapps")).unwrap();
        std::os::unix::fs::symlink(root.join("drive"), root.join("alias")).unwrap();
        let uid = std::fs::metadata(&root).unwrap().uid();
        let paths = ["drive", "alias", "gone"].map(|p| root.join(p).to_string_lossy().into_owned());
        let found = find(&paths, uid, btrfs);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].steamapps, root.join("drive/steamapps"));
    }

    fn shares(read_only: bool, spots: Vec<Spot>) -> Shares {
        Shares {
            nsenter: PathBuf::from("/usr/bin/nsenter"),
            mount: PathBuf::from("/usr/bin/mount"),
            umount: PathBuf::from("/usr/bin/umount"),
            idmap: "u:1000:987:1 g:1000:956:1".into(),
            read_only,
            libraries: vec![Library {
                steamapps: PathBuf::from("/run/media/bazzite/QEMU HARDDISK/steamapps"),
                key: "00aa".into(),
                spots,
            }],
        }
    }

    /// The unit mounts each library idmapped at its stage, hides the owner's own path, binds the
    /// stage and the seat's private folders into the seat, and keeps the owner's list read-only.
    /// `/run/media` stays hidden.
    #[test]
    fn the_drop_in_binds_each_library_with_private_folders() {
        let text = super::super::session::render_binds(
            Path::new("/b"),
            &id(),
            "pf-seat-1",
            Some(&shares(false, Vec::new())),
        );
        let seat = "/b/seats/0123456789abcdef0123456789abcdef";
        let stage = "/run/punktfunk/libraries/0123456789abcdef0123456789abcdef/00aa";
        let owner = "/run/media/bazzite/QEMU HARDDISK/steamapps";
        let mut expected = format!(
            "[Service]\n\
             TemporaryFileSystem=/run/media:ro\n\
             BindPaths={seat}/compatdata:/b/games/steamapps/compatdata\n\
             BindPaths={seat}/shadercache:/b/games/steamapps/shadercache\n\
             BindPaths={seat}/downloading:/b/games/steamapps/downloading\n\
             ExecStartPre=-+/usr/bin/nsenter -t 1 -m -- /usr/bin/mount --bind \
             -o \"nosuid,nodev,X-mount.idmap=u:1000:987:1 g:1000:956:1\" \"{owner}\" {stage}\n\
             ExecStopPost=-+/usr/bin/nsenter -t 1 -m -- /usr/bin/umount -l {stage}\n\
             InaccessiblePaths=\"-{owner}\"\n\
             BindPaths={stage}:{seat}/libraries/00aa/steamapps\n"
        );
        for name in ["compatdata", "shadercache", "downloading", "temp"] {
            expected.push_str(&format!(
                "BindPaths={seat}/private/00aa/{name}:{seat}/libraries/00aa/steamapps/{name}\n"
            ));
        }
        expected.push_str(&format!(
            "BindReadOnlyPaths=-{stage}/libraryfolders.vdf:\
             {seat}/libraries/00aa/steamapps/libraryfolders.vdf\n\
             BindPaths=-/dev/punktfunk/uinput-pf-seat-1:/dev/uinput\n"
        ));
        assert_eq!(text, expected);
    }

    /// Read-only, the mount gains `ro` and each spot gets the seat's copy; nothing else moves.
    #[test]
    fn a_read_only_library_mounts_ro_and_binds_each_spot() {
        let spots = [
            "common/Proton 10.0/dist.lock",
            "common/SteamLinuxRuntime_sniper/var",
        ];
        let shares_spots = spots
            .iter()
            .map(|path| Spot {
                path: PathBuf::from(path),
                dir: path.ends_with("var"),
            })
            .collect();
        let box_dir = Path::new("/b");
        let writable = unit_lines(box_dir, &id(), &shares(false, Vec::new()));
        let read_only = unit_lines(box_dir, &id(), &shares(true, shares_spots));
        let seat = "/b/seats/0123456789abcdef0123456789abcdef";
        let mut binds = String::new();
        for spot in spots {
            binds.push_str(&format!(
                "BindPaths=\"{seat}/private/00aa/{spot}\":\
                 \"{seat}/libraries/00aa/steamapps/{spot}\"\n"
            ));
        }
        let expected = writable
            .replace("-o \"nosuid,", "-o \"ro,nosuid,")
            .replace("BindReadOnlyPaths=", &format!("{binds}BindReadOnlyPaths="));
        assert_eq!(read_only, expected);
    }

    /// Each runtime's `var` and platform `.ref`, and each Proton's `dist.lock`, as they exist;
    /// no link is followed. The seat's copies are its own and keep what it wrote.
    #[test]
    fn the_spots_are_what_a_game_writes_in_a_library() {
        let temp = tempfile::tempdir().unwrap();
        let steamapps = temp.path().join("steamapps");
        let common = steamapps.join("common");
        for dir in [
            "SteamLinuxRuntime_sniper/var",
            "SteamLinuxRuntime_sniper/sniper_platform_1/files",
            "SteamLinuxRuntime_sniper/sniper_platform_2/files",
            "SteamLinuxRuntime_soldier/soldier_platform_1/files",
            "SteamLinuxRuntime_linkedfiles/platform",
            "Proton 10.0",
            "Proton - Experimental",
            "Proton linked lock",
            "Some Game/var",
        ] {
            std::fs::create_dir_all(common.join(dir)).unwrap();
        }
        for file in [
            "SteamLinuxRuntime_sniper/sniper_platform_1/files/.ref",
            "SteamLinuxRuntime_soldier/soldier_platform_1/files/.ref",
            "Proton 10.0/dist.lock",
        ] {
            std::fs::write(common.join(file), "").unwrap();
        }
        let link = |target: &str, at: &str| {
            std::os::unix::fs::symlink(common.join(target), common.join(at)).unwrap()
        };
        link("SteamLinuxRuntime_sniper", "SteamLinuxRuntime_linked");
        link("Proton 10.0", "Proton linked");
        link(
            "SteamLinuxRuntime_sniper/sniper_platform_1/files",
            "SteamLinuxRuntime_linkedfiles/platform/files",
        );
        link("Proton 10.0/dist.lock", "Proton linked lock/dist.lock");

        let found = spots(&steamapps);
        let want = [
            ("common/Proton 10.0/dist.lock", false),
            ("common/SteamLinuxRuntime_sniper/var", true),
            (
                "common/SteamLinuxRuntime_sniper/sniper_platform_1/files/.ref",
                false,
            ),
            (
                "common/SteamLinuxRuntime_soldier/soldier_platform_1/files/.ref",
                false,
            ),
        ]
        .map(|(path, dir)| Spot {
            path: PathBuf::from(path),
            dir,
        });
        assert_eq!(found, want);

        let private = temp.path().join("private");
        let seat = me(temp.path());
        make_spots(&private, &found, &seat).unwrap();
        let lock = private.join("common/Proton 10.0/dist.lock");
        assert!(private.join("common/SteamLinuxRuntime_sniper/var").is_dir());
        assert_eq!(std::fs::metadata(&lock).unwrap().uid(), seat.uid);
        std::fs::write(&lock, "held").unwrap();
        make_spots(&private, &found, &seat).unwrap();
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), "held");
    }

    /// The box's Seat default decides; without one the libraries stay writable.
    #[test]
    fn the_box_s_seat_default_makes_the_libraries_read_only() {
        let temp = tempfile::tempdir().unwrap();
        assert!(writable(temp.path()));
        let path = temp.path().join(pf_host_config::seat_defaults::FILE);
        for library_writable in [false, true] {
            let defaults = pf_host_config::seat_defaults::SeatDefaults {
                end_on_game_exit: true,
                max_mode: None,
                library_writable,
            };
            std::fs::write(&path, defaults.to_json().to_string()).unwrap();
            assert_eq!(writable(temp.path()), library_writable);
        }
        std::fs::write(&path, "{}").unwrap();
        assert!(writable(temp.path()));
    }

    fn steam_file(home: &Path, dir: &str) -> PathBuf {
        home.join(".local/share/Steam").join(dir).join(LIST)
    }

    /// Both of the seat's lists gain the box's libraries and lose a stale drive; Steam's own
    /// entry stays, and an absent copy stays absent.
    #[test]
    fn the_seat_s_lists_name_the_box_s_libraries() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let own = home
            .join(".local/share/Steam")
            .to_string_lossy()
            .into_owned();
        let before = steam::library_folders_vdf(&[
            own.clone(),
            "/run/media/system/QEMU_HARDDISK".to_owned(),
        ]);
        for dir in ["steamapps", "config"] {
            std::fs::create_dir_all(steam_file(&home, dir).parent().unwrap()).unwrap();
            std::fs::write(steam_file(&home, dir), &before).unwrap();
        }
        let seat = me(&home);
        let library = Library {
            steamapps: PathBuf::from("/x/steamapps"),
            key: "00aa".into(),
            spots: Vec::new(),
        };
        register(
            Path::new("/b"),
            &id(),
            &seat,
            std::slice::from_ref(&library),
        );
        let after = steam::library_folders_vdf(&[
            own.clone(),
            "/b/seats/0123456789abcdef0123456789abcdef/libraries/00aa".to_owned(),
        ]);
        for dir in ["steamapps", "config"] {
            assert_eq!(
                std::fs::read_to_string(steam_file(&home, dir)).unwrap(),
                after
            );
        }

        std::fs::remove_file(steam_file(&home, "config")).unwrap();
        register(Path::new("/b"), &id(), &seat, &[]);
        assert!(!steam_file(&home, "config").exists());
        let text = std::fs::read_to_string(steam_file(&home, "steamapps")).unwrap();
        assert_eq!(steam::library_paths(&text), [own]);
    }

    /// Root never follows a link the seat planted on the way to its lists.
    #[test]
    fn a_link_in_the_seat_s_tree_is_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let elsewhere = temp.path().join("elsewhere");
        let target = elsewhere.join("steamapps").join(LIST);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "\"libraryfolders\"\n{\n}\n").unwrap();
        std::fs::create_dir_all(home.join(".local/share")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, home.join(".local/share/Steam")).unwrap();
        let seat = me(temp.path());
        let merge = |_: &str| Some("changed".to_owned());
        assert!(merge_list(&home, "steamapps", &seat, merge).is_err());

        std::fs::remove_file(home.join(".local/share/Steam")).unwrap();
        std::fs::create_dir_all(home.join(".local/share/Steam/steamapps")).unwrap();
        std::os::unix::fs::symlink(&target, steam_file(&home, "steamapps")).unwrap();
        assert!(merge_list(&home, "steamapps", &seat, merge).is_err());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "\"libraryfolders\"\n{\n}\n"
        );
    }
}
