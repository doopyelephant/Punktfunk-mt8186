//! Hold-Select→Guide and the Select chords: pure state over button transitions and a clock.

use punktfunk_core::input::gamepad as wire;
use std::time::{Duration, Instant};

/// Hold Select alone this long for a synthetic host Guide ([`SelectGesture`]). The physical
/// Guide never reaches the host cleanly where the local shell owns it.
pub(super) const GUIDE_HOLD: Duration = Duration::from_millis(350);

/// Delay between a held-back Select tap's press and its scheduled release. Core folds
/// per-transition sends into one `GamepadState`; down+up in one window vanish.
pub(super) const TAP_PRESS: Duration = Duration::from_millis(50);

/// What a Select+button chord on a forwarded pad asks the client for.
///
/// [`Ring`](Self::Ring) withholds the press it takes, because A is the dial's own confirm. That
/// is why the client says whether anything is listening
/// ([`GamepadService::set_chords_live`](super::GamepadService::set_chords_live)): a chord
/// nothing receives would eat a face button the game was owed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectChord {
    /// Select+A — open the quick-action dial. The A press is withheld.
    Ring,
    /// Select+X — step the stats overlay's tier. Both presses still reach the game.
    Stats,
}

/// What a button pressed with Select already held asks for, whether or not the guide gesture
/// is on. Select-first only: A is the ring's own confirm, and X is a face button a game wants.
/// Once the hold became Guide, both belong to whatever that opened on the host.
pub(super) fn select_chord(held: &[u32], bit: u32, select_as_guide: bool) -> Option<SelectChord> {
    if select_as_guide || !held.contains(&wire::BTN_BACK) {
        return None;
    }
    match bit {
        wire::BTN_A => Some(SelectChord::Ring),
        wire::BTN_X => Some(SelectChord::Stats),
        _ => None,
    }
}

/// Hold-Select→guide ([`GUIDE_HOLD`]). Pure: transitions + clock emit `(bit, down)`
/// pairs. Select alone is pending; another button already down passes through (the
/// escape chord ends in Select). A button while pending makes it a real Select (deferred
/// down first). Past [`GUIDE_HOLD`] it is synthetic Guide until release; earlier it is a
/// tap — press on release, up [`TAP_PRESS`] later (back-to-back folds into nothing).
#[derive(Default)]
pub(super) struct SelectGesture {
    pending_since: Option<Instant>,
    pub(super) as_guide: bool,
    release_due: Option<Instant>,
    /// Ring chord: the pending press never went out, so the release is dropped here.
    swallowed: bool,
}

impl SelectGesture {
    /// Returns true when the press is held back. `alone` = no other button held.
    pub(super) fn on_select_down(
        &mut self,
        now: Instant,
        alone: bool,
        out: &mut Vec<(u32, bool)>,
    ) -> bool {
        // A previous tap's scheduled release is still owed: lift it before the new press.
        if self.release_due.take().is_some() {
            out.push((wire::BTN_BACK, false));
        }
        if alone {
            self.pending_since = Some(now);
            return true;
        }
        false
    }

    /// Ring chord: a pending Select never goes out, so its later release is dropped too.
    pub(super) fn swallow_for_ring(&mut self) {
        if self.pending_since.take().is_some() {
            self.swallowed = true;
        }
    }

    /// Pending Select is real after all — deferred down goes out before the new button.
    pub(super) fn on_other_down(&mut self, out: &mut Vec<(u32, bool)>) {
        if self.pending_since.take().is_some() {
            out.push((wire::BTN_BACK, true));
        }
    }

    /// True when the gesture owned this release (caller skips the normal button-up).
    pub(super) fn on_select_up(&mut self, now: Instant, out: &mut Vec<(u32, bool)>) -> bool {
        if self.swallowed {
            self.swallowed = false;
            return true;
        }
        if self.as_guide {
            self.as_guide = false;
            out.push((wire::BTN_GUIDE, false));
            return true;
        }
        if self.pending_since.take().is_some() {
            out.push((wire::BTN_BACK, true));
            self.release_due = Some(now + TAP_PRESS);
            return true;
        }
        false
    }

    pub(super) fn poll(&mut self, now: Instant, out: &mut Vec<(u32, bool)>) {
        if let Some(since) = self.pending_since {
            if now.duration_since(since) >= GUIDE_HOLD {
                self.pending_since = None;
                self.as_guide = true;
                out.push((wire::BTN_GUIDE, true));
            }
        }
        if let Some(due) = self.release_due {
            if now >= due {
                self.release_due = None;
                out.push((wire::BTN_BACK, false));
            }
        }
    }

    /// Slot close / disarm: nothing may stay down (or owed) on the wire.
    pub(super) fn flush(&mut self, out: &mut Vec<(u32, bool)>) {
        self.pending_since = None;
        self.swallowed = false;
        if self.as_guide {
            self.as_guide = false;
            out.push((wire::BTN_GUIDE, false));
        }
        if self.release_due.take().is_some() {
            out.push((wire::BTN_BACK, false));
        }
    }
}

#[cfg(test)]
mod select_gesture_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_while_select_is_pending_swallows_both() {
        let mut g = SelectGesture::default();
        let mut out = Vec::new();
        let t = Instant::now();
        assert!(g.on_select_down(t, true, &mut out));
        g.swallow_for_ring();
        assert!(out.is_empty(), "the pending press stays swallowed: {out:?}");
        assert!(
            g.on_select_up(t, &mut out),
            "the release is owned, not forwarded"
        );
        assert!(out.is_empty(), "…and emits nothing: {out:?}");
    }

    #[test]
    fn select_then_a_or_x_is_a_chord_without_the_guide_gesture() {
        let held = |b: u32| select_chord(&[wire::BTN_BACK], b, false);
        assert_eq!(held(wire::BTN_A), Some(SelectChord::Ring));
        assert_eq!(held(wire::BTN_X), Some(SelectChord::Stats));
        // Every other face button is the game's, held Select or not.
        assert_eq!(held(wire::BTN_B), None);
        assert_eq!(held(wire::BTN_Y), None);
        assert_eq!(
            select_chord(&[wire::BTN_LB, wire::BTN_BACK], wire::BTN_A, false),
            Some(SelectChord::Ring),
            "other buttons held alongside do not disqualify it"
        );
        assert_eq!(select_chord(&[], wire::BTN_A, false), None, "A alone");
        assert_eq!(
            select_chord(&[wire::BTN_A], wire::BTN_BACK, false),
            None,
            "A first"
        );
        assert_eq!(
            select_chord(&[wire::BTN_BACK], wire::BTN_X, true),
            None,
            "Select became Guide"
        );
    }

    #[test]
    fn tap_delivers_press_then_scheduled_release() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out), "not held back");
        assert!(out.is_empty(), "a held-back press sends nothing yet");
        let up = t + Duration::from_millis(120);
        assert!(g.on_select_up(up, &mut out));
        assert_eq!(out, vec![(wire::BTN_BACK, true)]);
        out.clear();
        g.poll(up + TAP_PRESS - Duration::from_millis(1), &mut out);
        assert!(out.is_empty(), "release went out early");
        g.poll(up + TAP_PRESS, &mut out);
        assert_eq!(out, vec![(wire::BTN_BACK, false)]);
    }

    #[test]
    fn hold_becomes_guide_down_until_release() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        g.poll(t + GUIDE_HOLD - Duration::from_millis(1), &mut out);
        assert!(out.is_empty(), "guide fired inside the threshold");
        g.poll(t + GUIDE_HOLD, &mut out);
        assert_eq!(out, vec![(wire::BTN_GUIDE, true)]);
        out.clear();
        g.poll(t + GUIDE_HOLD * 4, &mut out);
        assert!(out.is_empty());
        assert!(g.on_select_up(t + GUIDE_HOLD * 5, &mut out));
        assert_eq!(out, vec![(wire::BTN_GUIDE, false)]);
    }

    #[test]
    fn second_button_makes_pending_select_real() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        g.on_other_down(&mut out);
        assert_eq!(out, vec![(wire::BTN_BACK, true)]);
        out.clear();
        assert!(!g.on_select_up(t + Duration::from_millis(200), &mut out));
        assert!(out.is_empty());
        g.poll(t + GUIDE_HOLD * 2, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn select_inside_a_combo_passes_through() {
        let mut g = SelectGesture::default();
        let mut out = Vec::new();
        assert!(!g.on_select_down(Instant::now(), false, &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn quick_repress_lifts_owed_release_first() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        assert!(g.on_select_up(t + Duration::from_millis(80), &mut out));
        out.clear();
        assert!(g.on_select_down(t + Duration::from_millis(100), true, &mut out));
        assert_eq!(out, vec![(wire::BTN_BACK, false)]);
    }

    #[test]
    fn flush_lifts_synthetic_guide_and_owed_release() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        g.poll(t + GUIDE_HOLD, &mut out);
        out.clear();
        g.flush(&mut out);
        assert_eq!(out, vec![(wire::BTN_GUIDE, false)]);
        out.clear();
        assert!(g.on_select_down(t, true, &mut out));
        assert!(g.on_select_up(t + Duration::from_millis(80), &mut out));
        out.clear();
        g.flush(&mut out);
        assert_eq!(out, vec![(wire::BTN_BACK, false)]);
        out.clear();
        assert!(g.on_select_down(t, true, &mut out));
        g.flush(&mut out);
        assert!(out.is_empty(), "a never-sent pending Select ghosted a send");
    }
}
