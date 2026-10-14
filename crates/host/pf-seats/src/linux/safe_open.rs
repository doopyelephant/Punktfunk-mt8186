//! How root reads a file in a tree another uid can write: the box's, a seat's home, the owner's.
//!
//! The last step is always `O_NOFOLLOW | O_NONBLOCK`, so a link is refused and a FIFO or device
//! answers at once instead of stalling the supervisor. Only a regular file with one link and
//! within its size cap is read. A directory inside such a tree is opened the same way, one
//! component at a time ([`dir_at`], [`walk`]); a path is followed only up to a directory root
//! owns.

use rustix::fs::{Mode, OFlags};
use std::io::Read as _;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

/// A directory whose own path only root can change: `/var/lib/punktfunk`, a user's home.
pub(super) fn dir(path: &Path) -> std::io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    Ok(rustix::fs::open(path, flags, Mode::empty())?)
}

/// `name` in `at`, a directory and not a link. `None` when absent.
pub(super) fn dir_at(at: impl AsFd, name: &str) -> std::io::Result<Option<OwnedFd>> {
    open_at(at, name, OFlags::DIRECTORY)
}

fn open_at(at: impl AsFd, name: &str, extra: OFlags) -> std::io::Result<Option<OwnedFd>> {
    let flags = extra | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    match rustix::fs::openat(at, name, flags, Mode::empty()) {
        Ok(fd) => Ok(Some(fd)),
        Err(e) if e == rustix::io::Errno::NOENT => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `name` in `at` as a regular single-link file of at most `max` bytes. `None` when absent.
pub(super) fn file_at(
    at: impl AsFd,
    name: &str,
    max: u64,
) -> std::io::Result<Option<(std::fs::File, std::fs::Metadata)>> {
    let Some(fd) = open_at(at, name, OFlags::NOCTTY)? else {
        return Ok(None);
    };
    let file = std::fs::File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() > 1 || meta.len() > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{name} is not a plain single-link file within {max} bytes"),
        ));
    }
    Ok(Some((file, meta)))
}

/// The bytes of `name` in `at`, as [`file_at`] opens it. A file that grows past `max` while it
/// is read is refused.
pub(super) fn read_at(
    at: impl AsFd,
    name: &str,
    max: u64,
) -> std::io::Result<Option<(Vec<u8>, std::fs::Metadata)>> {
    let Some((file, meta)) = file_at(at, name, max)? else {
        return Ok(None);
    };
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{name} grew beyond {max} bytes"),
        ));
    }
    Ok(Some((bytes, meta)))
}

/// [`read_at`] for the last component of `path`, its directory opened by path.
pub(super) fn read(path: &Path, max: u64) -> std::io::Result<Option<(Vec<u8>, std::fs::Metadata)>> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} names no file", path.display()),
        ));
    };
    match dir(parent) {
        Ok(at) => read_at(at, name, max),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// `names` below `at`, one directory at a time, none of them a link. `None` when one is absent.
pub(super) fn walk(at: OwnedFd, names: &[&str]) -> std::io::Result<Option<OwnedFd>> {
    let mut at = at;
    for name in names {
        match dir_at(&at, name)? {
            Some(next) => at = next,
            None => return Ok(None),
        }
    }
    Ok(Some(at))
}
