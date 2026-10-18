//! The owner's host behind a door. The door opens no device, encodes nothing and runs no plugin:
//! the owner's host does, in the owner's row. So the console's calls about the host that streams
//! (its GPU, displays, hooks, recordings, Moonlight pairing, plugins) go there on the admin lane.
//! Every other lane, and every route not named in [`owner_route`], stays on the door.

use super::auth::AuthLane;
use super::shared::*;
use axum::extract::Request;
use axum::http::{header, Method};
use axum::middleware::Next;

/// What the door does with an admin call to one of the owner's host's routes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    /// The owner's host answers alone.
    Forward,
    /// The owner's host answers, and the door applies the same write to its own copy: the
    /// door's second-device rule and pairing hooks read it, and so do the seats' trust copies.
    Both,
}

/// How the owner's host handles `method` `rest` (the path under `/api/v1/`), if it does.
fn owner_route(method: &Method, rest: &str) -> Option<Owner> {
    let write = !matches!(*method, Method::GET | Method::HEAD);
    let segments: Vec<&str> = rest.split(['/', '?']).collect();
    // A dot segment stays on the door: no hop may resolve it onto a route this table never names.
    let dot = |s: &&str| {
        let s = s.to_ascii_lowercase().replace("%2e", ".");
        s == "." || s == ".."
    };
    if segments.iter().any(dot) {
        return None;
    }
    match segments[..] {
        ["display", "settings" | "layout"] | ["hooks"] => {
            Some(if write { Owner::Both } else { Owner::Forward })
        }
        ["gpus", ..]
        | ["session", "settings"]
        | ["host", "audio", "apps"]
        | ["display", "state" | "monitors" | "release" | "presets", ..]
        | ["stats", "capture" | "recordings", ..]
        | ["pair", ..]
        | ["clients", ..]
        | ["plugins"]
        | ["plugins", _, "ui-credential"]
        | ["plugins", _, "ui", ..]
        | ["plugin-access", ..] => Some(Owner::Forward),
        _ => None,
    }
}

/// The owner's host's answer to a call, or why there is none.
pub(super) enum Answer {
    /// No owner row: the door answers as the box's host did before it.
    NoOwner,
    /// The owner's session is stopped. It was asked to start.
    Starting,
    /// The owner's host didn't answer.
    Down,
    Reply(u16, Option<String>, Vec<u8>),
}

/// `method` `target` (under `/api/v1/`, query included) on the owner's host. `start` asks a
/// stopped owner's session to start; a health check never does.
pub(super) async fn call(
    method: &str,
    target: String,
    content_type: Option<String>,
    body: Vec<u8>,
    start: bool,
) -> Answer {
    let seats = tokio::task::spawn_blocking(crate::seats::snapshot)
        .await
        .unwrap_or_default();
    let Some(row) = seats.owner().cloned() else {
        return Answer::NoOwner;
    };
    if super::profiles::needs_start(&row.runtime.state) {
        if !start {
            return Answer::Down;
        }
        super::profiles::start_seat(row.id.as_str());
        return Answer::Starting;
    }
    let method = method.to_string();
    let forwarded = tokio::task::spawn_blocking(move || {
        crate::seats::forward(&row, &method, &target, content_type.as_deref(), body)
    })
    .await;
    match forwarded {
        Ok(Some((status, content_type, bytes))) => Answer::Reply(status, content_type, bytes),
        _ => Answer::Down,
    }
}

impl Answer {
    /// The answer as the console reads it. The owner's host is the box operator's own, so its
    /// content type passes as it came.
    pub(super) fn into_response(self) -> Response {
        match self {
            Answer::Reply(status, content_type, bytes) => {
                let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
                let mut resp = (status, bytes).into_response();
                if let Some(value) =
                    content_type.and_then(|c| header::HeaderValue::from_str(&c).ok())
                {
                    resp.headers_mut().insert(header::CONTENT_TYPE, value);
                }
                resp
            }
            Answer::Starting => api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The owner's session is starting. Try again in a moment.",
            ),
            Answer::Down | Answer::NoOwner => api_error(
                StatusCode::BAD_GATEWAY,
                "The owner's host didn't answer. Try again in a moment.",
            ),
        }
    }
}

/// axum's default request limit, which the seat proxy takes too: a policy, a hook list, a PIN,
/// a plugin page's form.
const BODY_LIMIT: usize = 2 * 1024 * 1024;

/// On a door, an admin call to the owner's host's routes ([`owner_route`]) goes to that host. A
/// door with no owner row answers itself.
pub(super) async fn to_the_owner(req: Request, next: Next) -> Response {
    let admin = req.extensions().get::<AuthLane>() == Some(&AuthLane::Admin);
    let how = req
        .uri()
        .path()
        .strip_prefix("/api/v1/")
        .and_then(|rest| owner_route(req.method(), rest));
    let Some(how) = how.filter(|_| admin && crate::seats::is_door()) else {
        return next.run(req).await;
    };
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, BODY_LIMIT).await else {
        return api_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    };
    let target = parts
        .uri
        .path_and_query()
        .and_then(|p| p.as_str().strip_prefix("/api/v1/"))
        .unwrap_or_default()
        .to_string();
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let answer = call(
        parts.method.as_str(),
        target,
        content_type,
        bytes.to_vec(),
        true,
    )
    .await;
    match answer {
        Answer::NoOwner => next.run(Request::from_parts(parts, bytes.into())).await,
        Answer::Reply(status, ..) if how == Owner::Both && (200..300).contains(&status) => {
            let own = next.run(Request::from_parts(parts, bytes.into())).await;
            if !own.status().is_success() {
                tracing::warn!(status = %own.status(), "door kept no copy of the owner's write");
            }
            answer.into_response()
        }
        answer => answer.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The door forwards what the owner's host does, keeps its own copy of what the door also
    /// reads, and keeps pairing, profiles, seats and the library proxy to itself.
    #[test]
    fn the_door_forwards_only_the_owner_s_host_routes() {
        let get = |rest| owner_route(&Method::GET, rest);
        let put = |rest| owner_route(&Method::PUT, rest);
        assert_eq!(put("display/settings"), Some(Owner::Both));
        assert_eq!(get("display/settings"), Some(Owner::Forward));
        assert_eq!(put("hooks"), Some(Owner::Both));
        assert_eq!(get("display/state"), Some(Owner::Forward));
        assert_eq!(
            put("display/presets/a"),
            Some(Owner::Forward),
            "the owner mints preset ids"
        );
        assert_eq!(put("gpus/preference"), Some(Owner::Forward));
        assert_eq!(get("plugins"), Some(Owner::Forward));
        assert_eq!(get("plugins/steam/ui-credential"), Some(Owner::Forward));
        assert_eq!(get("plugins/steam/ui/index.html"), Some(Owner::Forward));
        assert_eq!(get("plugins/steam/ui/../../native/clients"), None);
        assert_eq!(get("plugins/steam/ui/%2E%2e/x"), None);
        for door in [
            "display/clients/abc",
            "native/clients",
            "profiles",
            "profiles/a/proxy/library",
            "library",
            "plugins/steam",
            "host/settings",
            "diagnostics",
            "logs",
            "status",
        ] {
            assert_eq!(get(door), None, "{door}");
            assert_eq!(put(door), None, "{door}");
        }
    }
}
