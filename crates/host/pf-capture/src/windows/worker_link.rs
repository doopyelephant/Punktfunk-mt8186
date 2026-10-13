//! The host's end of one capture worker's control channel: the two pipes of
//! [`pf_driver_proto::worker`] and, as a [`Peer`], the process behind them.
//!
//! [`Link`] keeps one request in flight and waits a bound for its reply. The worker is less
//! privileged than this process: every reply is checked before use, a worker that answers
//! late or with the wrong bytes is ended, and nothing it sends is waited on without a bound.
//! A stream that stops being messages ends the channel; the next request then reads the
//! worker as exited.
//!
//! Generic over the pipes and the process, so the tests stand in for a worker on every
//! target; `wgc.rs` hands over the real ones. Evidence: `design/windows-wgc-capture.md`
//! §4.3, §4.7, §4.13.

// Off Windows only the tests read this module.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use std::io::{Read, Write};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bytemuck::Pod;
use pf_driver_proto::worker::{self as proto, kind};

pub(crate) type Message = (proto::Frame, Vec<u8>);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The worker's process, as its link uses it.
pub(crate) trait Peer {
    /// End the worker now. It holds no state of ours: a session starts another.
    fn kill(&self);
    /// The worker's exit code once it has exited.
    fn exit_code(&self) -> Option<u32>;
}

/// One worker's process and pipes. Closing the request pipe is how a worker is told to exit.
pub(crate) struct Link<P, W> {
    peer: P,
    /// The request pipe and the last sequence sent; `None` once closed. Held across a whole
    /// round trip, so one request is in flight at a time.
    requests: Mutex<Option<(W, u32)>>,
    replies: Mutex<Receiver<Message>>,
    events: Mutex<Receiver<Message>>,
}

/// Read whole messages off the worker's pipe until it closes or sends something that is not
/// one. Both queues are bounded: a worker that floods loses messages, not the host its memory.
fn pump(mut pipe: impl Read, replies: SyncSender<Message>, events: SyncSender<Message>) {
    loop {
        let mut head = [0u8; 16];
        if pipe.read_exact(&mut head).is_err() {
            return;
        }
        let frame = match proto::header(&head) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = ?e, "capture worker sent no message header");
                return;
            }
        };
        let mut body = vec![0u8; frame.len as usize];
        if pipe.read_exact(&mut body).is_err() {
            return;
        }
        let answer = frame.kind & proto::REPLY != 0 || frame.kind == kind::HELLO;
        let queue = if answer { &replies } else { &events };
        let _ = queue.try_send((frame, body));
    }
}

impl<P: Peer, W: Write> Link<P, W> {
    /// Take a started worker's pipes over and pump its replies on a thread of their own.
    pub(crate) fn open(peer: P, requests: W, replies: impl Read + Send + 'static) -> Result<Self> {
        let (reply_tx, reply_rx) = sync_channel(8);
        let (event_tx, event_rx) = sync_channel(64);
        std::thread::Builder::new()
            .name("pf-cw-replies".into())
            .spawn(move || pump(replies, reply_tx, event_tx))
            .context("spawn the capture worker reply thread")?;
        Ok(Self {
            peer,
            requests: Mutex::new(Some((requests, 0))),
            replies: Mutex::new(reply_rx),
            events: Mutex::new(event_rx),
        })
    }

    /// Trade hellos. A worker of another build is ended here.
    pub(crate) fn hello(&self, bound: Duration) -> Result<()> {
        let ours = proto::Hello::new(env!("CARGO_PKG_VERSION"));
        let theirs: proto::Hello = self.request(kind::HELLO, &ours, bound)?;
        if theirs != ours {
            self.peer.kill();
            bail!(
                "capture worker is another build ({:?})",
                String::from_utf8_lossy(&theirs.build).trim_end_matches('\0')
            );
        }
        Ok(())
    }

    pub(crate) fn peer(&self) -> &P {
        &self.peer
    }

    /// Send one request and wait `bound` for its reply. A hello is answered by a hello and is
    /// unnumbered on both sides. A worker that does not answer, or answers with the wrong
    /// bytes, is ended.
    pub(crate) fn request<Q: Pod, R: Pod>(&self, op: u32, body: &Q, bound: Duration) -> Result<R> {
        let mut requests = lock(&self.requests);
        let Some((pipe, last)) = requests.as_mut() else {
            bail!("capture worker is closed");
        };
        let (answer, seq) = if op == kind::HELLO {
            (kind::HELLO, 0)
        } else {
            // `0` is an event's sequence.
            *last = last.wrapping_add(1).max(1);
            (op | proto::REPLY, *last)
        };
        if let Err(e) = pipe.write_all(&proto::encode(op, seq, body)) {
            self.peer.kill();
            return Err(e).context("write to the capture worker");
        }
        let replies = lock(&self.replies);
        let deadline = Instant::now() + bound;
        loop {
            match replies.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok((frame, bytes)) if frame.kind == answer && frame.seq == seq => {
                    return proto::body(&bytes).ok_or_else(|| {
                        self.peer.kill();
                        anyhow!("capture worker answered {op:#x} with {} bytes", bytes.len())
                    });
                }
                // The answer to a request nobody waits for any more.
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    self.peer.kill();
                    bail!("capture worker did not answer {op:#x} within {bound:?}");
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("capture worker exited (code {:?})", self.peer.exit_code())
                }
            }
        }
    }

    /// The next event the worker sent, if any.
    pub(crate) fn event(&self) -> Option<Message> {
        lock(&self.events).try_recv().ok()
    }

    /// Close the request pipe: the worker exits when it does.
    pub(crate) fn close(&self) {
        drop(lock(&self.requests).take());
    }
}
