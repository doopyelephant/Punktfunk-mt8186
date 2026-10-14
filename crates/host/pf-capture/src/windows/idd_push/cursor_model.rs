//! Runs [`crate::cursor_state`] against the driver and the two shape sources.
//!
//! The model decides which source the client's shape is read from, and what the
//! driver's forward flag is across the secure desktop, a client's ask and a
//! monitor re-arrival. This file owns the sources (the GDI poller in
//! [`cursor_poll`], the driver's section in [`cursor`]), the IOCTLs and the log
//! line for each decision. It also hands the section to the driver at open.
//! The model's table test is the contract; nothing here decides.

use super::*;
use crate::cursor_state::{Action, Event, Source};

impl IddPushCapturer {
    /// Overlay source for [`Capturer::cursor`] and the shape the client draws.
    pub(super) fn live_cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        let alive = self.cursor_poll.as_ref().is_some_and(|p| p.alive());
        match self.cursor_model.source(alive) {
            Source::Poller => self.cursor_poll.as_ref().and_then(|p| p.read()),
            Source::Section { first } => {
                if first {
                    tracing::warn!(
                        target_id = self.target_id,
                        "cursor: the GDI shape poller is not running — degrading to the driver's \
                         hardware-cursor shm section for the rest of the session (alpha-only \
                         shapes: monochrome/masked cursors will look wrong)"
                    );
                }
                self.cursor_shared.as_mut().and_then(|c| c.read())
            }
            Source::None => None,
        }
    }

    /// Feed the model the secure desktop's state. UAC and Winlogon render through the OS
    /// software cursor, which a declared hardware cursor blocks.
    /// Must run every tick, including while frames are stalled.
    pub(super) fn poll_secure_desktop(&mut self) {
        // Nothing to stand down: no sender, or no declare on this target.
        if self.cursor_forward.is_none() || !self.cursor_model.declared() {
            return;
        }
        self.cursor_event(Event::Secure(pf_win_display::secure_desktop()));
    }

    /// Step the model, send the driver what it decided, and say so in the log.
    pub(super) fn cursor_event(&mut self, event: Event) {
        let action = self.cursor_model.step(event);
        let composite = self.cursor_model.composite();
        let forward = self.cursor_forward.as_ref();
        match action {
            Action::Nothing => {}
            Action::StandDown => {
                tracing::info!(
                    target_id = self.target_id,
                    "secure desktop (UAC/Winlogon) active — standing the IddCx hardware-cursor \
                     declare down so the OS software-cursor path can render it"
                );
                if let Some(Err(e)) = forward.map(|fwd| fwd(false)) {
                    tracing::warn!(
                        "secure-desktop cursor-forward stand-down failed (secure content may stay \
                         invisible this session): {e:#}"
                    );
                }
            }
            Action::Restore(flag) => {
                tracing::info!(
                    target_id = self.target_id,
                    "secure desktop dismissed — restoring the cursor render model"
                );
                if let Some(Err(e)) = forward.zip(flag).map(|(fwd, on)| fwd(on)) {
                    tracing::warn!(
                        "secure-desktop cursor-forward re-enable failed (client-drawn cursor \
                         may double with a composited one): {e:#}"
                    );
                }
            }
            Action::Flip(on) => {
                tracing::info!(
                    composite,
                    "cursor render model: the driver composites {}",
                    if composite {
                        "ON (capture model — blending the pointer into what it encodes)"
                    } else {
                        "OFF (client draws locally)"
                    }
                );
                if let Some(Err(e)) = forward.map(|fwd| fwd(on)) {
                    tracing::warn!(
                        composite,
                        error = %format!("{e:#}"),
                        "cursor render model: the driver did not take the flip"
                    );
                }
            }
            Action::Redeliver(on) => {
                let (Some(cs), Some(send)) =
                    (self.cursor_shared.as_ref(), self.cursor_sender.as_ref())
                else {
                    return;
                };
                tracing::info!(
                    target_id = self.target_id,
                    composite,
                    "cursor channel: re-delivering after the monitor re-arrival"
                );
                if !deliver_cursor_channel(&self.broker, self.target_id, cs, send) {
                    tracing::warn!(
                        target_id = self.target_id,
                        "cursor channel re-delivery failed after the re-arrival — a flip to the \
                         capture model will have no shape to blend"
                    );
                    return;
                }
                // Delivery starts the worker declared.
                if let Some(Err(e)) = forward.map(|fwd| fwd(on)) {
                    tracing::warn!(
                        composite,
                        error = %format!("{e:#}"),
                        "cursor render model not re-applied after the re-arrival"
                    );
                }
            }
        }
    }
}

/// Duplicate `cs` into WUDFHost and `IOCTL_SET_CURSOR_CHANNEL`.
/// `true` = adopted. Idempotent driver-side (replaced worker is stopped).
pub(super) fn deliver_cursor_channel(
    broker: &ChannelBroker,
    target_id: u32,
    cs: &cursor::CursorShared,
    send_cursor: &crate::CursorChannelSender,
) -> bool {
    let value = match broker.dup_into_public(cs.section_handle()) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("cursor section duplication failed (composited cursor stays): {e:#}");
            return false;
        }
    };
    let req = pf_driver_proto::control::SetCursorChannelRequest {
        target_id,
        _pad: 0,
        header_handle: value,
    };
    match send_cursor(&req) {
        Ok(()) => {
            tracing::info!(
                target_id,
                "IDD push(host): cursor channel delivered — driver declares the hardware cursor"
            );
            true
        }
        Err(e) => {
            broker.close_remote_public(value);
            tracing::warn!("cursor channel delivery failed (composited cursor stays): {e:#}");
            false
        }
    }
}
