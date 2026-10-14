//! Moves the supervisor's state out of the door's directory into [`super::STATE_DIR`].
//!
//! `seats`, `trust` and `games` belong where only root writes: the door's uid owns the box
//! directory and can rename anything in it. Each entry moves by `rename`, so both directories
//! must share a filesystem. An entry on another one, a mount point, or a target that already
//! holds files stops the move before anything moves, and the supervisor does not start. An entry
//! root does not own is the door's and stays where it is.
//!
//! Seats stop first: a seat user's home moves with `seats`. The environment files go too, so
//! every host comes back on the new paths. A second run, after a move that finished or not,
//! completes what is left and changes nothing else.

use super::accounts::{self, Passwd};
use super::{err, libraries, run, session};
use crate::backend::BackendError;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::Path;

const MOVED: [&str; 3] = ["seats", "trust", "games"];

/// Moves what `from` still holds into `to`, then points each seat account at its new home.
pub(super) fn move_state(from: &Path, to: &Path) -> Result<(), BackendError> {
    match std::fs::DirBuilder::new().mode(0o711).create(to) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(err("state_dir", format!("create {}: {e}", to.display()))),
    }
    let names = plan(from, to)?;
    if !names.is_empty() {
        tracing::info!(from = %from.display(), to = %to.display(), ?names, "moving the seats' state");
        stop_seats()?;
        clear_env_files();
        move_all(from, to, &names)?;
    }
    rehome(from, to)
}

/// The entries of `from` to move into `to`, every one checked before anything moves. An empty
/// one is removed instead, and so is an empty target.
pub(super) fn plan(from: &Path, to: &Path) -> Result<Vec<&'static str>, BackendError> {
    let refuse = |name: &str, why: &str| {
        err(
            "state_move",
            format!(
                "{} {why}; move it to {} by hand, then start the supervisor",
                from.join(name).display(),
                to.join(name).display()
            ),
        )
    };
    let io = |e: std::io::Error| err("state_move", format!("read {}: {e}", from.display()));
    let from_meta = match std::fs::symlink_metadata(from) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(e)),
    };
    let to_meta = std::fs::metadata(to).map_err(io)?;
    let mut out = Vec::new();
    for name in MOVED {
        let meta = match std::fs::symlink_metadata(from.join(name)) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io(e)),
        };
        if meta.uid() != to_meta.uid() {
            tracing::warn!(
                entry = name,
                uid = meta.uid(),
                "left in the box directory: not root's"
            );
            continue;
        }
        if !meta.is_dir() {
            return Err(refuse(name, "is not a directory"));
        }
        if meta.dev() != from_meta.dev() {
            return Err(refuse(name, "is a mount point"));
        }
        if meta.dev() != to_meta.dev() {
            return Err(refuse(name, "is on another filesystem"));
        }
        if std::fs::remove_dir(from.join(name)).is_ok() {
            continue;
        }
        match std::fs::remove_dir(to.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(refuse(name, "and its new place both hold files")),
        }
        out.push(name);
    }
    Ok(out)
}

pub(super) fn move_all(from: &Path, to: &Path, names: &[&str]) -> Result<(), BackendError> {
    for name in names {
        std::fs::rename(from.join(name), to.join(name)).map_err(|e| {
            err(
                "state_move",
                format!("move {}: {e}", from.join(name).display()),
            )
        })?;
    }
    Ok(())
}

/// Every seat unit, the owner's headless one too: their homes and trust copies move.
fn stop_seats() -> Result<(), BackendError> {
    run(
        "systemctl_failed",
        "systemctl",
        &["stop", "punktfunk-seat@*.service"],
    )
    .map(drop)
}

/// The supervisor writes each one afresh before a host reads it.
fn clear_env_files() {
    let Ok(entries) = std::fs::read_dir(session::ENV_DIR) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().ends_with(".env") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Points each seat account whose home is still under `from` at the same place under `to`, then
/// the paths its Steam wrote down.
fn rehome(from: &Path, to: &Path) -> Result<(), BackendError> {
    let old = from.join("seats");
    for user in accounts::all()?.into_iter().filter(accounts::is_seat) {
        let Ok(rest) = user.home.strip_prefix(&old) else {
            continue;
        };
        let home = to.join("seats").join(rest);
        accounts::quiesce(&user.name)?;
        let arg = home.to_string_lossy();
        run("usermod_failed", "usermod", &["-d", &arg, &user.name])?;
        follow(&Passwd { home, ..user }, from, to);
    }
    Ok(())
}

/// `text` with each moved entry's old path replaced by its new one.
fn moved(text: &str, from: &Path, to: &Path) -> String {
    MOVED.iter().fold(text.to_owned(), |text, name| {
        let (old, new) = (from.join(name), to.join(name));
        text.replace(&*old.to_string_lossy(), &new.to_string_lossy())
    })
}

/// Both of the seat's Steam library lists and the links under `~/.steam`, on the new paths. Root
/// writes in the seat's own tree, so nothing on the way is followed.
fn follow(user: &Passwd, from: &Path, to: &Path) {
    for dir in ["steamapps", "config"] {
        let merge = |text: &str| Some(moved(text, from, to));
        if let Err(error) = libraries::merge_list(&user.home, dir, user, merge) {
            tracing::warn!(account = %user.name, dir, %error, "steam library list keeps the old paths");
        }
    }
    if let Err(error) = relink(user, |target| moved(target, from, to)) {
        tracing::warn!(account = %user.name, %error, "steam links keep the old paths");
    }
}

fn relink(user: &Passwd, moved: impl Fn(&str) -> String) -> std::io::Result<()> {
    use rustix::fs::{AtFlags, Gid, Mode, OFlags, Uid};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let home = rustix::fs::open(&user.home, flags, Mode::empty())?;
    let steam = match rustix::fs::openat(&home, ".steam", flags | OFlags::NOFOLLOW, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) if e == rustix::io::Errno::NOENT => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(&steam)? {
        names.push(entry?.file_name().to_owned());
    }
    let owner = (Uid::from_raw(user.uid), Gid::from_raw(user.gid));
    for name in names {
        let Ok(target) = rustix::fs::readlinkat(&steam, name.as_c_str(), Vec::new()) else {
            continue;
        };
        let Ok(target) = target.into_string() else {
            continue;
        };
        let new = moved(&target);
        if new == target {
            continue;
        }
        rustix::fs::unlinkat(&steam, name.as_c_str(), AtFlags::empty())?;
        rustix::fs::symlinkat(new.as_str(), &steam, name.as_c_str())?;
        let nofollow = AtFlags::SYMLINK_NOFOLLOW;
        rustix::fs::chownat(
            &steam,
            name.as_c_str(),
            Some(owner.0),
            Some(owner.1),
            nofollow,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let (from, to) = (temp.path().join("box"), temp.path().join("state"));
        std::fs::create_dir_all(&to).unwrap();
        for name in MOVED {
            std::fs::create_dir_all(from.join(name).join("x")).unwrap();
        }
        std::fs::write(from.join("native-cert.pem"), "cert").unwrap();
        (temp, from, to)
    }

    /// Every entry moves once; the second run finds nothing and the box's own files stay.
    #[test]
    fn the_move_runs_once_and_a_second_run_changes_nothing() {
        let (_temp, from, to) = dirs();
        std::fs::create_dir(to.join("seats")).unwrap();
        let names = plan(&from, &to).unwrap();
        assert_eq!(names, MOVED);
        move_all(&from, &to, &names).unwrap();
        for name in MOVED {
            assert!(to.join(name).join("x").is_dir(), "{name}");
            assert!(!from.join(name).exists(), "{name}");
        }
        assert!(plan(&from, &to).unwrap().is_empty());
        assert!(from.join("native-cert.pem").is_file());
    }

    /// A target that already holds files stops the move before anything moves.
    #[test]
    fn a_target_with_files_stops_the_move() {
        let (_temp, from, to) = dirs();
        std::fs::create_dir_all(to.join("games").join("y")).unwrap();
        let error = plan(&from, &to).unwrap_err();
        assert_eq!(error.code, "state_move");
        for name in MOVED {
            assert!(from.join(name).join("x").is_dir(), "{name}");
        }
    }

    /// An empty leftover is removed, not moved over the state that is already in place.
    #[test]
    fn an_empty_leftover_is_removed() {
        let (_temp, from, to) = dirs();
        std::fs::remove_dir(from.join("trust").join("x")).unwrap();
        std::fs::create_dir_all(to.join("trust").join("y")).unwrap();
        assert_eq!(plan(&from, &to).unwrap(), ["seats", "games"]);
        assert!(!from.join("trust").exists());
        assert!(to.join("trust").join("y").is_dir());
    }

    /// Another filesystem stops the move with nothing moved. `/dev/shm` is a tmpfs where the
    /// temporary directory is not; without one this checks nothing.
    #[test]
    fn another_filesystem_stops_the_move() {
        let shm = Path::new("/dev/shm");
        let (_temp, from, _) = dirs();
        let Ok(to) = tempfile::tempdir_in(shm) else {
            return;
        };
        let dev = |p: &Path| std::fs::metadata(p).unwrap().dev();
        if dev(to.path()) == dev(&from) {
            return;
        }
        let error = plan(&from, to.path()).unwrap_err();
        assert!(
            error.message.contains("another filesystem"),
            "{}",
            error.message
        );
        for name in MOVED {
            assert!(from.join(name).join("x").is_dir(), "{name}");
            assert!(!to.path().join(name).exists(), "{name}");
        }
    }

    #[test]
    fn a_moved_path_is_rewritten_and_nothing_else() {
        let (from, to) = (
            Path::new("/var/lib/punktfunk"),
            Path::new("/var/lib/punktfunk-seats"),
        );
        let vdf = "\"path\"\t\t\"/var/lib/punktfunk/games\"\n\
                   \"path\"\t\t\"/var/lib/punktfunk/seats/s/home/.local/share/Steam\"\n\
                   \"path\"\t\t\"/run/media/x\"\n";
        assert_eq!(
            moved(vdf, from, to),
            "\"path\"\t\t\"/var/lib/punktfunk-seats/games\"\n\
             \"path\"\t\t\"/var/lib/punktfunk-seats/seats/s/home/.local/share/Steam\"\n\
             \"path\"\t\t\"/run/media/x\"\n"
        );
        assert_eq!(
            moved("/var/lib/punktfunk/profiles.json", from, to),
            "/var/lib/punktfunk/profiles.json"
        );
    }
}
