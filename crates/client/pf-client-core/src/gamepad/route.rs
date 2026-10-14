//! Where pad input goes, over plain values. [`Gates`] holds what the client asked for, one
//! field per `Ctl`; [`Gates::input_route`] reads them as one [`InputRoute`]. [`route`] turns one
//! transition on a forwarded pad into the [`Action`]s the worker performs, in order.
//!
//! No SDL type: the worker maps SDL events to [`PadInput`] and re-reads the pad when an action
//! asks it to. The table test below pins every route.

use super::select_gesture::{select_chord, SelectGesture};
use super::SelectChord;
use punktfunk_core::client::Sc2Gate;
use punktfunk_core::input::gamepad as wire;
use std::time::Instant;

/// The client's asks that decide where pad input goes. Each `Ctl` has its own caller, so the
/// fields are kept as sent and read together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Gates {
    /// Console navigation while no session is attached.
    pub(super) menu_mode: bool,
    /// The overlay owns the pads: neutral on the host, slots still open.
    pub(super) masked: bool,
    /// The in-stream ring polls the forwarded pads.
    pub(super) ring_nav: bool,
    /// Physical Guide and Quick Access reach the host; off, they stay with the local shell.
    pub(super) system_forward: bool,
    /// Something listens for a Select chord. Off, a chord would eat a press the game was owed.
    pub(super) chords_live: bool,
    /// Hold-Select is a synthetic host Guide ([`SelectGesture`]).
    pub(super) guide_gesture: bool,
}

impl Default for Gates {
    fn default() -> Gates {
        Gates {
            menu_mode: false,
            masked: false,
            ring_nav: false,
            system_forward: true,
            chords_live: false,
            guide_gesture: false,
        }
    }
}

/// Where pad input goes. Forwarded slots exist only with a session; menu pads only without.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InputRoute {
    /// No session, no menu: no pad is open.
    Closed,
    /// No session: every pad open and polled into menu events.
    Menu,
    /// No session, menu pads open, but the mask or the ring holds them: nothing moves.
    MenuHeld,
    /// Forwarded pads drive the host.
    Host,
    /// The overlay owns the pads: neutral on the host, slots open.
    Masked,
    /// The ring is up under the mask: forwarded pads polled into menu events.
    Ring,
    /// The ring without the mask: pads drive the host and the ring.
    HostAndRing,
}

/// The open pads menu navigation reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MenuReads {
    Nothing,
    /// What menu mode holds open.
    MenuPads,
    /// The forwarded slots, for the ring.
    Slots,
}

impl InputRoute {
    pub(super) fn menu_reads(self) -> MenuReads {
        match self {
            InputRoute::Menu => MenuReads::MenuPads,
            InputRoute::Ring | InputRoute::HostAndRing => MenuReads::Slots,
            InputRoute::Closed | InputRoute::MenuHeld | InputRoute::Host | InputRoute::Masked => {
                MenuReads::Nothing
            }
        }
    }
}

impl Gates {
    pub(super) fn input_route(&self, session: bool) -> InputRoute {
        match (session, self.masked, self.ring_nav) {
            (true, false, false) => InputRoute::Host,
            (true, true, false) => InputRoute::Masked,
            (true, true, true) => InputRoute::Ring,
            (true, false, true) => InputRoute::HostAndRing,
            (false, false, false) if self.menu_mode => InputRoute::Menu,
            (false, ..) if self.menu_mode => InputRoute::MenuHeld,
            (false, ..) => InputRoute::Closed,
        }
    }

    /// The typed plane's mask, system-button routing and chord, for a raw Steam Controller.
    pub(super) fn sc2(&self) -> Sc2Gate {
        Sc2Gate {
            masked: self.masked,
            system_local: !self.system_forward,
            chords: self.chords_live,
        }
    }
}

/// One transition on a forwarded pad, with SDL's button already a wire bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PadInput {
    Button {
        bit: u32,
        down: bool,
    },
    /// A Steam pad's trackpad click: a touch surface, never the button plane.
    Click {
        surface: u8,
        down: bool,
    },
    /// A stick, trigger, touch or motion sample.
    Analog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    /// A button transition on the wire.
    Wire { bit: u32, down: bool },
    /// The trackpad click on its surface.
    Click { surface: u8, down: bool },
    /// The analog sample on the wire.
    Forward,
    /// Zero what the host holds for this pad.
    Flush,
    /// The local Guide lifted: re-read the pad into [`PadRoute::adopt`].
    Readopt,
    /// A Select chord for the client.
    Chord(SelectChord),
    /// A press took the host path: check the escape chord.
    Escape,
    /// A release took the host path: let go of a broken escape hold.
    Rearm,
}

/// One forwarded slot's routing memory.
#[derive(Default)]
pub(super) struct PadRoute {
    /// Wire bits down on this pad, a withheld chord press included.
    held: Vec<u32>,
    /// The button a Ring chord withheld: its release does not go out either.
    swallow: Option<u32>,
    /// Guide is down and stays local. It is the local shell's chord key (Steam: Guide+A opens
    /// the QAM), so the rest of this pad stays off the wire until it lifts.
    guide_local: bool,
    pub(super) gesture: SelectGesture,
}

impl PadRoute {
    /// Every press the host holds for this pad, as releases: the gesture's synthetic Guide or
    /// owed tap first (neither is in `held`), then each held button. A withheld chord press is
    /// forgotten too: under the mask its own release never arrives, so it would eat the next.
    pub(super) fn release(&mut self) -> Vec<(u32, bool)> {
        let mut out = Vec::new();
        self.gesture.flush(&mut out);
        self.swallow = None;
        out.extend(self.held.drain(..).map(|b| (b, false)));
        out
    }

    /// The pad is ours again. What is `down` now (wire bits, trackpad clicks left out) is held
    /// without a wire press: an A that picked a QAM row must not fire in the game. False while
    /// a local Guide still holds the pad; true when the caller re-sends the sticks.
    pub(super) fn adopt(
        &mut self,
        down: impl IntoIterator<Item = u32>,
        guide_down: bool,
        system_forward: bool,
    ) -> bool {
        self.held.clear();
        self.guide_local = !system_forward && guide_down;
        if self.guide_local {
            return false;
        }
        let ours = |b: &u32| system_forward || !is_system(*b);
        self.held.extend(down.into_iter().filter(ours));
        true
    }
}

fn is_system(bit: u32) -> bool {
    matches!(bit, wire::BTN_GUIDE | wire::BTN_MISC1)
}

/// Where one transition on a forwarded pad goes. Under the mask nothing moves, and a local
/// Guide holds the rest of its pad until it lifts. A press meets the Select chord, then the
/// guide gesture, then the wire.
pub(super) fn route(
    input: PadInput,
    gates: &Gates,
    pad: &mut PadRoute,
    now: Instant,
) -> Vec<Action> {
    let guide = matches!(
        input,
        PadInput::Button {
            bit: wire::BTN_GUIDE,
            ..
        }
    );
    if gates.masked || (pad.guide_local && !guide) {
        return Vec::new();
    }
    match input {
        PadInput::Analog => vec![Action::Forward],
        PadInput::Click { surface, down } => vec![Action::Click { surface, down }],
        PadInput::Button { bit, down: true } => press(bit, gates, pad, now),
        PadInput::Button { bit, down: false } => release(bit, gates, pad, now),
    }
}

fn press(bit: u32, gates: &Gates, pad: &mut PadRoute, now: Instant) -> Vec<Action> {
    if !gates.system_forward && is_system(bit) {
        // Neutral for the hold, as under the mask: what is pressed with Guide is the shell's
        // chord, and one already held must not stay down host-side.
        if bit == wire::BTN_GUIDE {
            pad.guide_local = true;
            return vec![Action::Flush];
        }
        return Vec::new();
    }
    let mut out = Vec::new();
    let chord = gates
        .chords_live
        .then(|| select_chord(&pad.held, bit, pad.gesture.as_guide))
        .flatten();
    if let Some(chord) = chord {
        out.push(Action::Chord(chord));
        // A is the ring's own confirm, so the host never sees it. A pending Select goes with
        // it; one already on the wire is lifted by the ring's mask flush. Stats changes
        // nothing on the host, so that press carries on to the game.
        if chord == SelectChord::Ring {
            pad.gesture.swallow_for_ring();
            pad.swallow = Some(bit);
            pad.held.push(bit);
            return out;
        }
    }
    let mut due = Vec::new();
    let held_back = if !gates.guide_gesture {
        false
    } else if bit == wire::BTN_BACK {
        let alone = pad.held.is_empty();
        pad.gesture.on_select_down(now, alone, &mut due)
    } else {
        pad.gesture.on_other_down(&mut due);
        false
    };
    out.extend(
        due.into_iter()
            .map(|(bit, down)| Action::Wire { bit, down }),
    );
    // Chord bookkeeping sees the physical press even when the gesture holds it back.
    pad.held.push(bit);
    if !held_back {
        out.push(Action::Wire { bit, down: true });
    }
    out.push(Action::Escape);
    out
}

fn release(bit: u32, gates: &Gates, pad: &mut PadRoute, now: Instant) -> Vec<Action> {
    // The host never saw this Guide go down: its lift only hands the pad back.
    if bit == wire::BTN_GUIDE && pad.guide_local {
        return vec![Action::Readopt];
    }
    if !gates.system_forward && is_system(bit) {
        return Vec::new();
    }
    pad.held.retain(|&b| b != bit);
    if pad.swallow == Some(bit) {
        pad.swallow = None;
        return Vec::new();
    }
    let mut due = Vec::new();
    let owned =
        gates.guide_gesture && bit == wire::BTN_BACK && pad.gesture.on_select_up(now, &mut due);
    let mut out: Vec<Action> = due
        .into_iter()
        .map(|(bit, down)| Action::Wire { bit, down })
        .collect();
    if !owned {
        out.push(Action::Wire { bit, down: false });
    }
    out.push(Action::Rearm);
    out
}
