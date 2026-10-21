//! One session's scope: what it starts, what it holds, and its one teardown.
//!
//! A session spawns its side threads ([`SessionScope::thread`]) and tasks
//! ([`SessionScope::task`]) here and hands over its RAII guards ([`SessionScope::hold`]).
//! [`SessionScope::finish`] tears it down in this order:
//!
//! 1. end the lifecycle: `host_ended` on `Ok`, `host_error` on `Err`; the first reason wins;
//! 2. close the link `0 "done"` or `1 "error"`, which ends every task; a session that failed
//!    before it streamed is left open for its plane's typed refusal (`setup_failed_sentence`);
//! 3. join the side threads, newest first, within [`SIDE_THREAD_JOIN_GRACE`], else detach;
//! 4. enter [`Phase::Done`];
//! 5. drop the gamescope hold: the last one out hands the TV's gaming session back;
//! 6. drop the planes off the runtime's workers: their mic release is a PipeWire round trip;
//! 7. drop the held guards, newest first: the slot permit, held first, goes last.
//!
//! A scope dropped unfinished detaches its threads and runs 5–7.

use super::{link, planes::SessionPlanes};
use crate::compositor_route::GamescopeHold;
use crate::events::SessionEndReason;
use crate::lifecycle::{Lifecycle, Phase};

/// Audio/input join after close. Both poll `stop`: audio every ≤100 ms, input every ≤25 ms.
/// This only catches a wedge.
const SIDE_THREAD_JOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) struct SessionScope {
    life: Lifecycle,
    conn: link::SessionLink,
    threads: Vec<std::thread::JoinHandle<()>>,
    gamescope: Option<GamescopeHold>,
    planes: Option<SessionPlanes>,
    guards: Vec<Box<dyn Send>>,
}

impl SessionScope {
    /// `gamescope` is taken before the handshake resolves the compositor, so a session that
    /// fails there still hands the TV back.
    pub(super) fn new(life: Lifecycle, conn: link::SessionLink, gamescope: GamescopeHold) -> Self {
        SessionScope {
            life,
            conn,
            threads: Vec::new(),
            gamescope: Some(gamescope),
            planes: None,
            guards: Vec::new(),
        }
    }

    pub(super) fn life(&self) -> &Lifecycle {
        &self.life
    }

    pub(super) fn conn(&self) -> &link::SessionLink {
        &self.conn
    }

    /// A side thread the teardown joins.
    pub(super) fn thread(
        &mut self,
        name: &str,
        f: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<()> {
        let thread = std::thread::Builder::new().name(name.into()).spawn(f)?;
        self.threads.push(thread);
        Ok(())
    }

    /// A task of this session. It ends on the link's close, which the teardown makes, or on a
    /// lane the stream side drops; none is joined.
    pub(super) fn task(&self, f: impl std::future::Future<Output = ()> + Send + 'static) {
        tokio::spawn(f);
    }

    /// Held to step 7 of the teardown.
    pub(super) fn hold<G: Send + 'static>(&mut self, guard: G) {
        self.guards.push(Box::new(guard));
    }

    /// Held to step 6 of the teardown, after the input thread that routes through them.
    pub(super) fn set_planes(&mut self, planes: SessionPlanes) {
        self.planes = Some(planes);
    }

    pub(super) fn planes(&self) -> &SessionPlanes {
        self.planes
            .as_ref()
            .expect("a session mints its planes before it reads them")
    }

    /// The teardown, steps 1–7. The join is bounded: a stuck side thread must not hold the
    /// permit or the admission entry.
    pub(super) async fn finish(mut self, result: &anyhow::Result<()>) {
        let ok = result.is_ok();
        // The plane that admitted a session refuses one that never streamed, in its own words;
        // a close here would pre-empt that typed close.
        let refused_by_plane = !ok && self.life.phase() < Phase::Streaming;
        self.life.end(if ok {
            SessionEndReason::HostEnded
        } else {
            SessionEndReason::HostError
        });
        if !refused_by_plane {
            self.conn
                .close(if ok { 0 } else { 1 }, if ok { b"done" } else { b"error" });
        }
        let threads = std::mem::take(&mut self.threads);
        let joined = tokio::task::spawn_blocking(move || {
            for thread in threads.into_iter().rev() {
                let _ = thread.join();
            }
        });
        if tokio::time::timeout(SIDE_THREAD_JOIN_GRACE, joined)
            .await
            .is_err()
        {
            // Input thread still owns the virtual pads (Windows: devnode + pad-index mailbox).
            // The next create on that index fails as already-owned until this thread returns.
            tracing::warn!(
                grace_s = SIDE_THREAD_JOIN_GRACE.as_secs(),
                "audio/input threads did not exit after the session stopped — detaching them. \
                 This session's virtual gamepads are STILL HELD by the detached input thread \
                 (devnode + pad-index mailbox on Windows), so a pad create on the same index \
                 will be refused as already-owned until it returns"
            );
        }
        self.life.enter(Phase::Done);
        self.release();
    }

    /// Steps 5–7. A second call finds nothing left.
    fn release(&mut self) {
        drop(self.gamescope.take());
        if let Some(planes) = self.planes.take() {
            off_workers(|| drop(planes));
        }
        while let Some(guard) = self.guards.pop() {
            drop(guard);
        }
    }
}

impl Drop for SessionScope {
    fn drop(&mut self) {
        self.release();
    }
}

/// `f` off the runtime's workers where the runtime allows it.
fn off_workers(f: impl FnOnce()) {
    match tokio::runtime::Handle::try_current().map(|rt| rt.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    /// A host link to a loopback client, the client's end, and the endpoints both live on.
    async fn loopback() -> (
        link::SessionLink,
        quinn::Connection,
        (quinn::Endpoint, quinn::Endpoint),
    ) {
        use punktfunk_core::quic::endpoint;
        let server = endpoint::server("127.0.0.1:0".parse().unwrap()).expect("server");
        let addr = server.local_addr().expect("server address");
        let client = endpoint::client_insecure().expect("client");
        let (host, peer) = tokio::join!(
            async { server.accept().await.expect("incoming").await },
            async { client.connect(addr, "punktfunk").expect("dial").await },
        );
        let host = host.expect("host side connects");
        let media = std::net::UdpSocket::bind("127.0.0.1:0").expect("media socket");
        let link = link::SessionLink::QuicV2(
            host.clone(),
            Arc::new(link::V2Link::new(host, Arc::new(media))),
        );
        (link, peer.expect("client side connects"), (server, client))
    }

    /// Records its name when dropped.
    struct Mark(&'static str, Arc<Mutex<Vec<&'static str>>>);

    impl Drop for Mark {
        fn drop(&mut self) {
            self.1.lock().unwrap().push(self.0);
        }
    }

    /// A guard outlives the side thread that may still use it: the join comes first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_joins_the_side_threads_before_it_drops_a_guard() {
        let (conn, peer, _endpoints) = loopback().await;
        let life = Lifecycle::new();
        let mut scope = SessionScope::new(life.clone(), conn, GamescopeHold::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        scope.hold(Mark("guard", order.clone()));
        let (stop, seen) = (life.stop_flag().clone(), order.clone());
        scope
            .thread("pf-scope-test", move || {
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                seen.lock().unwrap().push("thread");
            })
            .unwrap();
        scope.finish(&Ok(())).await;
        assert_eq!(*order.lock().unwrap(), ["thread", "guard"]);
        assert_eq!(life.reason(), Some(SessionEndReason::HostEnded));
        assert_eq!(life.phase(), Phase::Done);
        assert_eq!(peer_close(&peer).await, (0, b"done".to_vec()));
    }

    /// The host's application close as the client reads it: code and reason.
    async fn peer_close(peer: &quinn::Connection) -> (u64, Vec<u8>) {
        match peer.closed().await {
            quinn::ConnectionError::ApplicationClosed(c) => {
                (u64::from(c.error_code), c.reason.to_vec())
            }
            other => panic!("expected the host's close, got {other:?}"),
        }
    }

    /// A bring-up that fails leaves the link open for the typed refusal of the plane that
    /// admitted it, which carries the user's sentence. A stream that fails closes `1 "error"`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_bring_up_leaves_the_close_to_its_plane() {
        for (phase, closes) in [(Phase::BringUp, false), (Phase::Streaming, true)] {
            let (conn, peer, _endpoints) = loopback().await;
            let life = Lifecycle::new();
            life.enter(phase);
            let scope = SessionScope::new(life.clone(), conn.clone(), GamescopeHold::new());
            scope.finish(&Err(anyhow::anyhow!("no display"))).await;
            assert_eq!(
                life.reason(),
                Some(SessionEndReason::HostError),
                "{phase:?}"
            );
            assert_eq!(conn.close_reason().is_some(), closes, "{phase:?}");
            let setup_failed = punktfunk_core::reject::SETUP_FAILED_CLOSE_CODE;
            conn.close(setup_failed, b"The host's screen is asleep");
            let said = peer_close(&peer).await;
            match closes {
                true => assert_eq!(said, (1, b"error".to_vec())),
                false => assert_eq!(said.0, u64::from(setup_failed)),
            }
        }
    }
}
