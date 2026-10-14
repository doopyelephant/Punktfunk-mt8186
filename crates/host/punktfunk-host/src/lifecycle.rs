//! One session's life: whether it is ending, and why.
//!
//! Every thread and task of a session holds a clone. [`Lifecycle::end`] is the one writer:
//! it latches the reason, marks a deliberate end and raises `stop`. A reader that only
//! polls a flag takes [`Lifecycle::stop_flag`] or [`Lifecycle::quit_flag`]: the display
//! lease, the game lease, admission, the side threads.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use crate::events::SessionEndReason;

/// The flags every thread of a session reads, and the one place that writes them.
#[derive(Clone, Debug)]
pub struct Lifecycle {
    stop: Arc<AtomicBool>,
    /// Set with `stop` on a deliberate end: the display skips its keep-alive linger and the
    /// game lease opens no reconnect window.
    quit: Arc<AtomicBool>,
    /// [`SessionEndReason`] as `u8`; 0 until `end`. The first writer wins.
    reason: Arc<AtomicU8>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Lifecycle {
    pub fn new() -> Self {
        Self::with_quit(Arc::new(AtomicBool::new(false)))
    }

    /// A lifecycle whose deliberate-end flag is `quit`: a plane that keeps one host-wide
    /// (GameStream's `AppState::quit`) shares it with every session it serves.
    pub fn with_quit(quit: Arc<AtomicBool>) -> Self {
        Lifecycle {
            stop: Arc::new(AtomicBool::new(false)),
            quit,
            reason: Arc::new(AtomicU8::new(0)),
        }
    }

    /// End the session for `why`. The first reason wins, so the path that knows (a game
    /// exit, an operator stop, the peer's own close) calls before the generic close that
    /// follows it. Every call raises `stop`; a deliberate `why` raises `quit` too.
    pub fn end(&self, why: SessionEndReason) {
        let _ = self
            .reason
            .compare_exchange(0, why as u8, Ordering::SeqCst, Ordering::Relaxed);
        if why.deliberate() {
            self.quit.store(true, Ordering::SeqCst);
        }
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// `None` until `end`: a session summarised without one never reached its tail.
    pub fn reason(&self) -> Option<SessionEndReason> {
        SessionEndReason::from_u8(self.reason.load(Ordering::SeqCst))
    }

    pub fn stop_flag(&self) -> &Arc<AtomicBool> {
        &self.stop
    }

    pub fn quit_flag(&self) -> &Arc<AtomicBool> {
        &self.quit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REASONS: [SessionEndReason; 6] = [
        SessionEndReason::Local,
        SessionEndReason::GameExited,
        SessionEndReason::HostEnded,
        SessionEndReason::HostError,
        SessionEndReason::Lost,
        SessionEndReason::StoppedByOperator,
    ];

    /// The first reason is the one the summary reports; a later `end` still raises the flags.
    #[test]
    fn the_first_reason_wins_and_each_is_recorded_once() {
        for first in REASONS {
            for second in REASONS {
                let life = Lifecycle::new();
                assert!(!life.stopping());
                life.end(first);
                life.end(second);
                assert!(life.stopping(), "{first:?} then {second:?}");
                assert_eq!(life.reason(), Some(first), "{first:?} then {second:?}");
            }
        }
    }

    /// `quit` is what the display lease and the game lease read: a client's own stop, a
    /// game exit and an operator stop mean it; a lost link, a fault and a plain finish do not.
    #[test]
    fn a_deliberate_end_raises_quit_and_the_rest_do_not() {
        for why in REASONS {
            let life = Lifecycle::new();
            life.end(why);
            assert_eq!(
                life.quit_flag().load(Ordering::SeqCst),
                matches!(
                    why,
                    SessionEndReason::Local
                        | SessionEndReason::GameExited
                        | SessionEndReason::StoppedByOperator
                ),
                "{why:?}"
            );
        }
    }
}
