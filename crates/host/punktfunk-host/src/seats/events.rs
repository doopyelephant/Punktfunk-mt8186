//! The door's events and the seats': one bus for the console. The seats' hosts stream, so
//! their sessions, games, library and plugin changes happen there; the door pairs and places,
//! so knocks, grants and power happen here. Each side's events reach the other's bus: the
//! seats' into the door's, for the console; the door's into the owner's host, whose hooks run
//! in the owner's session. The two kind sets are disjoint, so nothing comes back around.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Event kinds the door hands to the owner's host: what happens on the door alone.
const RELAYED: &[&str] = &["pairing.*", "access.*", "host.*", "action.*", "update.*"];

/// Whether `kind` is the door's own, handed to the owner's host.
fn relayed(kind: &str) -> bool {
    RELAYED
        .iter()
        .any(|pattern| crate::events::kind_matches(pattern, kind))
}

/// How often the door looks for a running seat it has no tail on.
const TICK: Duration = Duration::from_secs(3);

/// Tails every running seat's events into this host's bus, for the host's lifetime.
pub(crate) fn fan_in() {
    std::thread::Builder::new()
        .name("seat-events".into())
        .spawn(|| {
            let cursors: Arc<Mutex<HashMap<String, u64>>> = Arc::default();
            let mut tails: HashMap<String, std::thread::JoinHandle<()>> = HashMap::new();
            loop {
                tails.retain(|_, tail| !tail.is_finished());
                // The ledger alone: asking each seat host would poll them all every tick.
                let ledger = super::ledger();
                for seat in &ledger.seats {
                    let id = seat.id.as_str().to_string();
                    if seat.runtime.state != pf_seats::RuntimeState::Running
                        || tails.contains_key(&id)
                    {
                        continue;
                    }
                    let Some(profile) = profile_of(seat) else {
                        continue;
                    };
                    let seat = seat.clone();
                    let cursors = Arc::clone(&cursors);
                    let tail = std::thread::Builder::new()
                        .name(format!("seat-events-{id}"))
                        .spawn(move || tail(&seat, profile, &cursors));
                    if let Ok(tail) = tail {
                        tails.insert(id, tail);
                    }
                }
                std::thread::sleep(TICK);
            }
        })
        .ok();
}

/// The profile whose host `seat` runs: its own, or the owner's for the owner's row.
fn profile_of(seat: &pf_seats::Seat) -> Option<String> {
    let profiles = crate::profiles::Profiles::load_with(None, None).list();
    profiles
        .iter()
        .find(|p| match &p.os_account {
            crate::profiles::OsAccount::Seat { seat: Some(id) } => id == seat.id.as_str(),
            crate::profiles::OsAccount::Operator => seat.owner,
            _ => false,
        })
        .map(|p| p.id.to_string())
}

/// Reads `seat`'s events from where this host last saw them, into this host's bus as
/// `profile`'s, until the connection ends. The next tick starts it over.
fn tail(seat: &pf_seats::Seat, profile: String, cursors: &Mutex<HashMap<String, u64>>) {
    let id = seat.id.as_str().to_string();
    // Past every seq the seat has: only what happens from now on, never a replay.
    let since = cursors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .copied()
        .unwrap_or(u64::MAX);
    let Some(body) = super::tail(seat, &format!("events?since={since}")) else {
        return;
    };
    for (event, data) in frames(std::io::BufReader::new(body)) {
        if event == "dropped" || event == "live" {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<crate::events::HostEvent>(&data) else {
            continue;
        };
        cursors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), ev.seq);
        if !relayed(ev.kind.name()) {
            crate::events::bus().emit_from(ev.kind, Some(profile.clone()));
        }
    }
}

/// The `(event, data)` of each SSE frame in `reader`, in order. Ends with the stream.
fn frames(reader: impl std::io::BufRead) -> impl Iterator<Item = (String, String)> {
    let mut lines = reader.lines();
    std::iter::from_fn(move || {
        let (mut event, mut data) = (String::new(), Vec::new());
        loop {
            let line = lines.next()?.ok()?;
            if line.is_empty() {
                if event.is_empty() && data.is_empty() {
                    continue;
                }
                return Some((event, data.join("\n")));
            }
            if let Some(value) = line.strip_prefix("event:") {
                event = value.trim().to_string();
            } else if let Some(value) = line.strip_prefix("data:") {
                data.push(value.strip_prefix(' ').unwrap_or(value).to_string());
            }
        }
    })
}

/// The one relayed kind that wakes a stopped owner's session: someone is waiting on an answer.
const WAKES: &str = "pairing.pending";

/// How long a knock waits for the owner's host to come up before its hooks are skipped.
const WAKE_WAIT: Duration = Duration::from_secs(120);

/// Hands each of this door's own events to the owner's host, for the owner's hooks. A knock
/// wakes a stopped owner's session and waits for its host; any other event a stopped or silent
/// host misses.
pub(crate) async fn relay_to_owner() {
    let mut rx = crate::events::bus().subscribe_live();
    loop {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        if ev.seat.is_some() || !relayed(ev.kind.name()) {
            continue;
        }
        let Ok(body) = serde_json::to_vec(&serde_json::json!({ "event": ev })) else {
            continue;
        };
        let wakes = ev.kind.name() == WAKES;
        tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + WAKE_WAIT;
            let mut asked = false;
            loop {
                let ledger = super::ledger();
                let Some(owner) = ledger.owner() else { return };
                if owner.runtime.state == pf_seats::RuntimeState::Running {
                    let call = crate::mgmt::owner::Forwarded {
                        method: "POST".into(),
                        target: "events/relay".into(),
                        content_type: Some("application/json".into()),
                        body,
                        device: None,
                        stream: false,
                        start: false,
                    };
                    if super::forward(owner, call).is_none() {
                        tracing::debug!(
                            kind = ev.kind.name(),
                            "event not relayed to the owner's host"
                        );
                    }
                    return;
                }
                if !wakes || std::time::Instant::now() >= deadline {
                    return;
                }
                if !asked {
                    asked = true;
                    let started = super::call(pf_seats::ipc::Command::Start {
                        id: owner.id.clone(),
                    });
                    if let Err(error) = started {
                        tracing::warn!(code = ?error.code, "owner's session did not start for a knock: {}", error.message);
                        return;
                    }
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_on_the_blank_line_and_join_data_lines() {
        let raw = "event: live\ndata: {\"live\":true}\n\nid: 7\nevent: library.changed\ndata: {\"a\":1,\ndata: \"b\":2}\n\n\n";
        let got: Vec<_> = frames(std::io::Cursor::new(raw)).collect();
        assert_eq!(
            got,
            vec![
                ("live".to_string(), "{\"live\":true}".to_string()),
                (
                    "library.changed".to_string(),
                    "{\"a\":1,\n\"b\":2}".to_string()
                ),
            ]
        );
    }

    /// What the door originates goes to the owner; what a seat originates comes to the door.
    /// A kind in both would come back around for good.
    #[test]
    fn the_two_kind_sets_are_disjoint() {
        for kind in [
            "pairing.pending",
            "access.granted",
            "host.started",
            "update.applied",
        ] {
            assert!(relayed(kind), "{kind}");
        }
        for kind in [
            "session.started",
            "stream.started",
            "game.exited",
            "library.changed",
            "plugins.changed",
            "settings.changed",
            "display.created",
        ] {
            assert!(!relayed(kind), "{kind}");
        }
    }
}
