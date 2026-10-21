//! One session's scope: what it starts, what it holds, and its one teardown.
//!
//! A session spawns its side threads ([`SessionScope::thread`]) and tasks
//! ([`SessionScope::task`]) here and hands over its RAII guards ([`SessionScope::hold`]).
//! [`SessionScope::finish`] tears it down in this order:
//!
//! 1. end the lifecycle: `host_ended` on `Ok`, `host_error` on `Err`; the first reason wins;
//! 2. close the link `0 "done"` or `1 "error"`; the close ends every task;
//! 3. join the side threads, newest first, within [`SIDE_THREAD_JOIN_GRACE`], else detach;
//! 4. enter [`Phase::Done`];
//! 5. drop the gamescope hold: the last one out hands the TV's gaming session back;
//! 6. drop the planes off the runtime's workers: their mic release is a PipeWire round trip;
//! 7. drop the held guards, newest first: the slot permit, held first, goes last.
//!
//! A scope dropped unfinished detaches its threads and runs 5–7.

use super::{link, SessionPlanes};
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
        self.life.end(if ok {
            SessionEndReason::HostEnded
        } else {
            SessionEndReason::HostError
        });
        self.conn
            .close(if ok { 0 } else { 1 }, if ok { b"done" } else { b"error" });
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
                "audio/input threads did not exit after the connection closed — detaching them. \
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
        match peer.closed().await {
            quinn::ConnectionError::ApplicationClosed(c) => {
                assert_eq!(u64::from(c.error_code), 0);
                assert_eq!(&c.reason[..], b"done");
            }
            other => panic!("expected the host's close, got {other:?}"),
        }
    }
}
