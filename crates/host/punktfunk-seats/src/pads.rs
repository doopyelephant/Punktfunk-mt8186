//! The supervisor's pad broker socket (`design/seat-pad-broker.md`): a running seat's host asks
//! here for a pad, and the supervisor builds and relays it (`pf_inject::pad_broker`).
//!
//! The socket file is world-connectable; the gate is `SO_PEERCRED`: a peer whose uid is not a
//! running seat's (the owner's row included) is closed unanswered. A seat holds at most
//! [`MAX_PER_SEAT`] pads and [`vhci::MAX_PER_SEAT`] USB devices; past that the answer is
//! `capacity`. A pad lives as long as its relay thread, a USB device as long as its connection:
//! the seat hanging up, or its host dying, ends both. A relayed pad and an attached USB socket
//! also sit in systemd's fd store, so a supervisor restart picks them up again ([`resume`]).

use crate::vhci;
use pf_inject::pad_broker::{self, BrokeredPad, PadKind, Request, Status};
use pf_seats::linux::{socket, LinuxBackend};
use pf_seats::SeatService;
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

pub const SOCKET_PATH: &str = pf_paths::seat::PADS_SOCKET;
/// Relayed pads one seat may hold at once: its players' controllers, with room for a swap.
pub const MAX_PER_SEAT: usize = 8;
/// A seat sends its request at once; one that doesn't is not a seat host.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// One file per relayed pad, `<account> <index> <kind>`, for `punktfunk-seats list`.
const RELAYED: &str = "/run/punktfunk/pads/relayed";

/// Pads held per account, for the life of their relays.
static HELD: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(Mutex::default);

/// One of an account's pad slots, given back on drop.
struct Slot(String);

impl Slot {
    fn take(account: &str) -> Option<Slot> {
        admit(&mut HELD.lock().unwrap_or_else(|e| e.into_inner()), account)
            .then(|| Slot(account.to_owned()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        release(&mut HELD.lock().unwrap_or_else(|e| e.into_inner()), &self.0);
    }
}

/// Counts one more pad for `account`; `false` at the cap.
fn admit(held: &mut HashMap<String, usize>, account: &str) -> bool {
    let count = held.entry(account.to_owned()).or_insert(0);
    if *count >= MAX_PER_SEAT {
        return false;
    }
    *count += 1;
    true
}

fn release(held: &mut HashMap<String, usize>, account: &str) {
    if let Some(count) = held.get_mut(account) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            held.remove(account);
        }
    }
}

/// Relays again every pad systemd kept across a supervisor restart, and watches again every
/// USB socket. First thing at start: until then the kept fds would leak into a child.
pub fn resume() {
    let _ = std::fs::remove_dir_all(RELAYED);
    let (pads, sockets) = pad_broker::kept();
    for kept in pads {
        let Some(slot) = Slot::take(&kept.account) else {
            pad_broker::forget(&kept.name);
            continue;
        };
        tracing::info!(
            account = kept.account,
            pad = kept.kind.label(),
            index = kept.index,
            "seat pad kept across a restart"
        );
        let spawned = std::thread::Builder::new()
            .name("seat-pad".into())
            .spawn(move || {
                let _slot = slot;
                run_relay(
                    &kept.name,
                    &kept.account,
                    kept.kind,
                    kept.index,
                    kept.pad,
                    kept.relay,
                );
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "kept seat pad thread did not start");
        }
    }
    for (port, recorded, sock) in sockets {
        tracing::info!(port, "seat USB device kept across a restart");
        let spawned = std::thread::Builder::new()
            .name("seat-usb".into())
            .spawn(move || {
                vhci::watch(port, recorded, sock);
                tracing::info!(port, "seat USB device gone");
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, port, "kept seat USB thread did not start");
        }
    }
}

/// Relays `pad` until its seat hangs up, listed for `list` and in systemd's fd store meanwhile.
fn run_relay(
    name: &str,
    account: &str,
    kind: PadKind,
    index: u8,
    pad: BrokeredPad,
    relay: UnixDatagram,
) {
    let record =
        pad_broker::parse_store_name(name).map(|(serial, ..)| Path::new(RELAYED).join(serial));
    if let Some(record) = &record {
        let written = std::fs::create_dir_all(RELAYED)
            .and_then(|()| std::fs::write(record, format!("{account} {index} {}\n", kind as u8)));
        if let Err(error) = written {
            tracing::debug!(%error, "seat pad not listed");
        }
    }
    pad_broker::relay(pad, relay);
    pad_broker::forget(name);
    if let Some(record) = &record {
        let _ = std::fs::remove_file(record);
    }
    tracing::info!(account, pad = kind.label(), index, "seat pad released");
}

/// `list`'s lines for what the seats hold: `pad <account> <kind> #<index>` and
/// `usb <account> port <n>`.
pub fn listing() -> Vec<String> {
    let rows = |dir: &str| {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                Some((
                    e.file_name().into_string().ok()?,
                    std::fs::read_to_string(e.path()).ok()?,
                ))
            })
            .collect::<Vec<_>>()
    };
    let mut out: Vec<String> = rows(RELAYED)
        .into_iter()
        .filter_map(|(_, row)| {
            let mut words = row.split_whitespace();
            let (account, index) = (words.next()?, words.next()?);
            let kind = PadKind::from_wire(words.next()?.parse().ok()?)?;
            Some(format!("pad {account} {} #{index}", kind.label()))
        })
        .collect();
    out.extend(rows(vhci::MAP).into_iter().filter_map(|(port, row)| {
        Some(format!(
            "usb {} port {port}",
            row.split_whitespace().next()?
        ))
    }));
    out.sort();
    out
}

/// Binds `path`, replacing a stale socket file, open to every local user: the peer's uid is
/// what admits a request.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let listener = socket::bind_fresh(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// At the supervisor's stop (`ungate`): without the socket, the box's own hidraw nodes open to
/// everyone again, as the distro's rules have them.
pub fn unbind() {
    let _ = std::fs::remove_file(SOCKET_PATH);
    refresh_hidraw();
}

/// Has udev apply its rules to every hidraw node again. `65-punktfunk-seats.rules` closes the
/// box's own nodes to other users while the pads socket exists; a node made before that keeps
/// what it got until an event reaches it.
pub fn refresh_hidraw() {
    let Ok(entries) = std::fs::read_dir("/sys/class/hidraw") else {
        return;
    };
    for entry in entries.flatten() {
        if let Err(error) = std::fs::write(entry.path().join("uevent"), "change") {
            tracing::warn!(%error, node = %entry.file_name().to_string_lossy(),
                "hidraw node not refreshed");
        }
    }
}

/// Serves `listener` until the process ends: one thread per pad, for the pad's life.
pub fn serve(listener: UnixListener, service: Arc<SeatService<LinuxBackend>>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, "pads socket accept");
                continue;
            }
        };
        let service = Arc::clone(&service);
        let spawned = std::thread::Builder::new()
            .name("seat-pad".into())
            .spawn(move || handle(&service, stream));
        if let Err(error) = spawned {
            tracing::warn!(%error, "pads socket thread did not start");
        }
    }
}

fn handle(service: &SeatService<LinuxBackend>, stream: UnixStream) {
    let uid = match socket::peer_uid(&stream) {
        Ok(uid) => uid,
        Err(error) => {
            tracing::warn!(%error, "pads socket peer credentials");
            return;
        }
    };
    let Some(account) = service.backend().running_account(uid) else {
        tracing::warn!(uid, "pads socket refused a peer that is no running seat");
        return;
    };
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let (request, fd) = match pad_broker::read_request(&stream) {
        Ok(read) => read,
        Err(error) => {
            tracing::debug!(error = %format!("{error:#}"), account, "pads socket request");
            return;
        }
    };
    match request {
        Request::Create { kind, index } => create(stream, &account, kind, index),
        Request::Attach { devid, speed } => attach(stream, &account, fd, devid, speed),
        Request::Detach { port } => {
            let status = match vhci::detach(&account, port) {
                Ok(()) => Status::Detached,
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), account, port, "seat vhci detach refused");
                    Status::Failed
                }
            };
            answer(&stream, &account, status);
        }
    }
}

fn answer(stream: &UnixStream, account: &str, status: Status) {
    if let Err(error) = pad_broker::reply(stream, status, 0, None) {
        tracing::debug!(error = %format!("{error:#}"), account, "pads socket answer");
    }
}

/// A uinput or uhid pad, relayed until the seat hangs up.
fn create(stream: UnixStream, account: &str, kind: PadKind, index: u8) {
    let Some(_slot) = Slot::take(account) else {
        tracing::warn!(
            account,
            cap = MAX_PER_SEAT,
            "seat asked for one pad too many"
        );
        answer(&stream, account, Status::Capacity);
        return;
    };
    let pad = match pad_broker::build(kind, index, account) {
        Ok(pad) => pad,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), account, pad = kind.label(),
                "seat pad not made");
            answer(&stream, account, Status::Failed);
            return;
        }
    };
    let (ours, theirs) = match pad_broker::relay_pair() {
        Ok(pair) => pair,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), account, "seat pad relay not made");
            answer(&stream, account, Status::Failed);
            return;
        }
    };
    if let Err(error) = pad_broker::reply(&stream, Status::Created, 0, Some(theirs.as_fd())) {
        tracing::warn!(error = %format!("{error:#}"), account, "seat pad not handed over");
        return;
    }
    drop(theirs);
    drop(stream);
    tracing::info!(account, pad = kind.label(), index, "seat pad made");
    let name = pad_broker::store_name(kind, index, account);
    pad_broker::keep(&name, &pad, &ours);
    run_relay(&name, account, kind, index, pad, ours);
}

/// The seat's usbip socket on a vhci port, held until its connection ends.
fn attach(stream: UnixStream, account: &str, sock: Option<OwnedFd>, devid: u32, speed: u32) {
    let Some(sock) = sock else {
        tracing::warn!(account, "seat asked to attach without a socket");
        answer(&stream, account, Status::Failed);
        return;
    };
    let port = match vhci::attach(account, sock.as_fd(), devid, speed) {
        Ok(Some(port)) => port,
        Ok(None) => {
            tracing::warn!(
                account,
                cap = vhci::MAX_PER_SEAT,
                "seat asked for one USB device too many"
            );
            answer(&stream, account, Status::Capacity);
            return;
        }
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), account, "seat USB device not attached");
            answer(&stream, account, Status::Failed);
            return;
        }
    };
    if let Err(error) = pad_broker::reply(&stream, Status::Attached, port, None) {
        tracing::warn!(error = %format!("{error:#}"), account, port, "seat USB attach not answered");
    }
    drop(stream);
    tracing::info!(account, port, "seat USB device attached");
    let recorded = sock.as_raw_fd();
    pad_broker::keep_usb(port, recorded, sock.as_fd());
    vhci::watch(port, recorded, sock);
    tracing::info!(account, port, "seat USB device gone");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seat_holds_at_most_the_cap_and_gets_slots_back() {
        let mut held = HashMap::new();
        for _ in 0..MAX_PER_SEAT {
            assert!(admit(&mut held, "pf-seat-1"));
        }
        assert!(!admit(&mut held, "pf-seat-1"), "one too many");
        assert!(
            admit(&mut held, "pf-seat-2"),
            "another seat has its own cap"
        );
        release(&mut held, "pf-seat-1");
        assert!(admit(&mut held, "pf-seat-1"));
        for _ in 0..=MAX_PER_SEAT {
            release(&mut held, "pf-seat-1");
        }
        assert!(
            !held.contains_key("pf-seat-1"),
            "a seat with no pads is forgotten"
        );
    }

    #[test]
    fn the_pads_socket_is_open_to_every_local_user() {
        let dir = std::env::temp_dir().join(format!("pf-pads-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pads.sock");
        let _listener = bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o666);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
