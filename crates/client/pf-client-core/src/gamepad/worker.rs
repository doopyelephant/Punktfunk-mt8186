//! The SDL side of [`GamepadService`](super::GamepadService): the pad list, forwarded
//! slots, input forwarding, menu navigation, and the rumble/HID-output render. [`run`]
//! drives a [`Worker`] on its own thread; [`GamepadPump`](super::GamepadPump) drives one
//! from the caller's loop.

use super::ds5::Ds5Feedback;
use super::route::{route, Action, Gates, MenuReads, MenuSettle, PadInput, PadRoute};
use super::select_gesture::TAP_PRESS;
use super::{is_steam_deck, set_valve_hidapi, Ctl, SelectChord, DISCONNECT_HOLD};
use crate::menu_nav::{
    ring_sector, MenuEvent, MenuNav, MenuPulse, MenuSample, PadBattery, PadInfo,
};
use punktfunk_core::client::{ActuatorQuirks, NativeClient};
use punktfunk_core::config::GamepadPref;
use punktfunk_core::input::{gamepad as wire, InputEvent, InputKind};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// SDL gyro is rad/s and accel is m/s²; the DualSense report wants the wire LSBs
/// ([`wire::MOTION_GYRO_LSB_PER_DEG_S`] / [`wire::MOTION_ACCEL_LSB_PER_G`]).
const GYRO_LSB_PER_RAD_S: f32 =
    wire::MOTION_GYRO_LSB_PER_DEG_S as f32 * 180.0 / std::f32::consts::PI;
const ACCEL_LSB_PER_G: f32 = wire::MOTION_ACCEL_LSB_PER_G as f32;
const G: f32 = 9.80665;

/// L1+R1+Start+Select: raises the UI escape (on a desktop the presenter releases capture,
/// which masks forwarding until capture returns). A hold of [`DISCONNECT_HOLD`]
/// disconnects, mask or not. Not Guide/QAM — those pass through to the host.
const ESCAPE_CHORD: [sdl3::gamepad::Button; 4] = [
    sdl3::gamepad::Button::LeftShoulder,
    sdl3::gamepad::Button::RightShoulder,
    sdl3::gamepad::Button::Start,
    sdl3::gamepad::Button::Back,
];

/// Deck actuator keepalive, declared as [`ActuatorQuirks`] at slot open. The built-in
/// actuator decays inside SDL's ~2 s rumble resend, and an identical `set_rumble` is a
/// no-op, so a steady level pulses unless re-kicked sub-decay; 40 ms matches SDL's
/// Steam-Controller driver. The engine owns timing and 1-LSB jitter.
const DECK_RUMBLE_KEEPALIVE_MS: u16 = 40;

/// Open-pad battery re-read. 15 s is coarser than a percent move, finer than the
/// worker loop; the read is a cached HID report.
const BATTERY_POLL: Duration = Duration::from_secs(15);

/// Open-pad power report. `None` is wired-no-battery, error, unknown, and SDL's `-1`
/// percent — draw no battery, never 0 % (that looks like empty).
fn battery_of(pad: &sdl3::gamepad::Gamepad) -> Option<PadBattery> {
    use sdl3::joystick::PowerLevel;
    let info = pad.power_info();
    let charging = match info.state {
        PowerLevel::OnBattery => false,
        PowerLevel::Charging | PowerLevel::Charged => true,
        PowerLevel::NoBattery | PowerLevel::Error | PowerLevel::Unknown => return None,
    };
    if info.percentage < 0 {
        return None;
    }
    Some(PadBattery {
        percent: info.percentage.min(100) as u8,
        charging,
    })
}

/// Log every joystick SDL sees, gamepad or not. A controller SDL has no mapping
/// for never becomes a pad, so this line is the only place it shows up.
fn log_joysticks(subsystem: &sdl3::GamepadSubsystem) {
    let mut count = 0i32;
    // SAFETY: the subsystem exists, so SDL's joystick code is initialised. The
    // call returns null or an array of `count` ids that the caller frees.
    let ids = unsafe { sdl3::sys::joystick::SDL_GetJoysticks(&mut count) };
    if ids.is_null() {
        return;
    }
    // SAFETY: non-null, `count` ids long, and not freed until the line below.
    let list = unsafe { std::slice::from_raw_parts(ids, count.max(0) as usize) }.to_vec();
    // SAFETY: `ids` came from `SDL_GetJoysticks` and is freed exactly once.
    unsafe { sdl3::sys::stdinc::SDL_free(ids.cast()) };
    if list.is_empty() {
        tracing::info!("no controller seen at session start");
    }
    for id in list.into_iter().map(sdl3::joystick::JoystickId::from) {
        tracing::info!(
            id = format_args!(
                "{:04x}:{:04x}",
                subsystem.vendor_for_id(id).unwrap_or(0),
                subsystem.product_for_id(id).unwrap_or(0)
            ),
            name = %subsystem.name_for_id(id).unwrap_or_default(),
            path = %subsystem.path_for_id(id).unwrap_or_default(),
            gamepad = subsystem.is_gamepad(id),
            "controller seen at session start"
        );
    }
}

fn pref_for_type(t: sdl3::gamepad::GamepadType) -> GamepadPref {
    use sdl3::gamepad::GamepadType as T;
    match t {
        T::PS5 => GamepadPref::DualSense,
        T::PS4 => GamepadPref::DualShock4,
        T::XboxOne => GamepadPref::XboxOne,
        T::NintendoSwitchPro => GamepadPref::SwitchPro,
        // A single Joy-Con is half a pad and stays on the Xbox 360 fallback.
        T::NintendoSwitchJoyconPair => GamepadPref::JoyConPair,
        _ => GamepadPref::Xbox360,
    }
}

/// Pads whose own identity SDL's type cannot name: no Valve, 8BitDo or HORI type, the Edge
/// reads as a PS5 and the Elite as an Xbox One. The host then builds that identity, so the
/// extras land natively.
fn pref_for_ids(vid: u16, pid: u16) -> Option<GamepadPref> {
    match (vid, pid) {
        (0x28DE, 0x1205) => Some(GamepadPref::SteamDeck),
        // The 2015 controller: cable, dongle, and Bluetooth under either firmware's id.
        (0x28DE, 0x1102 | 0x1142 | 0x1105 | 0x1106) => Some(GamepadPref::SteamController),
        (0x054C, 0x0DF2) => Some(GamepadPref::DualSenseEdge),
        // Elite Series 1, Series 2 USB, Bluetooth and BLE.
        (0x045E, 0x02E3 | 0x0B00 | 0x0B05 | 0x0B22) => Some(GamepadPref::XboxElite),
        // 8BitDo in its own HID mode; X-input mode is `310B` and stays Xbox 360.
        (0x2DC8, 0x6012) => Some(GamepadPref::EightBitDoUltimate2),
        (0x2DC8, 0x6003 | 0x6006) => Some(GamepadPref::EightBitDoPro2),
        (0x2DC8, 0x6009) => Some(GamepadPref::EightBitDoPro3),
        (0x0F0D, 0x01AB | 0x0196) => Some(GamepadPref::HoripadSteam),
        (0x057E, 0x2069) => Some(GamepadPref::Switch2Pro),
        (0x057E, 0x2073) => Some(GamepadPref::Switch2GameCube),
        _ => crate::sc2_capture::pref_for(vid, pid),
    }
}

/// Kind declared in [`InputKind::GamepadArrival`]: an explicit setting emulates that
/// pad on every slot; `Auto` keeps per-pad detection. Applied per pad, not only in
/// Hello — the host builds each virtual device from arrival. Local feedback still
/// uses the physical kind (the controller in hand, not the host's pretence).
fn declared_kind(setting: GamepadPref, physical: GamepadPref) -> GamepadPref {
    match setting {
        GamepadPref::Auto => physical,
        explicit => explicit,
    }
}

/// A synthetic tap's wire events as `(bit, down, after)`: each press [`TAP_PRESS`] after
/// the one before, then every release together, last pressed first. Apart, because core
/// folds per-transition sends into one state, and because a chord's first button has to be
/// held before its second counts.
fn tap_events(presses: &[u32]) -> Vec<(u32, bool, Duration)> {
    let downs = presses.iter().zip(0u32..);
    let mut events: Vec<_> = downs.map(|(&b, i)| (b, true, TAP_PRESS * i)).collect();
    let up = TAP_PRESS * presses.len() as u32;
    events.extend(presses.iter().rev().map(|&b| (b, false, up)));
    events
}

/// What SDL's HIDAPI driver calls the Deck's built-in controller.
const DECK_NAME: &str = "Steam Deck";

/// A Deck's pad list: Steam Input's pads are shadows of the built-in controls and of each
/// external pad SDL already lists (the session clears Steam's device filter). The built-in
/// controller is listed once: the raw 28DE:1205 when a session has it, else one Steam pad.
/// A Steam Controller is listed raw too: its SDL drivers stay on while the Deck's is off.
fn fold_deck_shadows(list: &mut Vec<PadInfo>) {
    let mut stand_in = !list.iter().any(raw_deck);
    list.retain(|p| !p.steam_virtual || std::mem::take(&mut stand_in));
}

/// The Deck's built-in controller listed raw (`28DE:1205`: SDL's in-session driver, or the
/// kernel's while Steam is not running), not Steam Input's pad for it.
fn raw_deck(p: &PadInfo) -> bool {
    !p.steam_virtual && p.pref == GamepadPref::SteamDeck
}

/// What menu mode opens of `ids`. While a Steam Input pad is listed in `order`, the first one
/// stands in for the raw Deck controller: after a session the raw pad stays listed until its
/// removal arrives, and opening it clears lizard mode. With Steam not running, it opens.
fn menu_pads(ids: Vec<u32>, order: &[u32], info: impl Fn(u32) -> Option<PadInfo>) -> Vec<u32> {
    let steam = |p: &PadInfo| p.steam_virtual && p.pref == GamepadPref::SteamDeck;
    let Some(stand_in) = order
        .iter()
        .copied()
        .find(|&id| info(id).is_some_and(|p| steam(&p)))
    else {
        return ids;
    };
    let mut out = Vec::new();
    for id in ids {
        let id = if info(id).is_some_and(|p| raw_deck(&p)) {
            stand_in
        } else {
            id
        };
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Lowest free wire index, or `None` when every slot is taken. Lowest-free keeps indices
/// stable: a disconnect frees only its own index, so a game never sees players shuffle.
fn lowest_free_index(taken: &[u8]) -> Option<u8> {
    (0..punktfunk_core::input::MAX_PADS as u8).find(|i| !taken.contains(i))
}

/// One per-transition event tagged with wire pad index (`flags`). Core folds these into
/// seq'd [`GamepadState`](punktfunk_core::input::InputKind::GamepadState) keyed on that index.
fn send(connector: &NativeClient, kind: InputKind, code: u32, x: i32, pad: u8) {
    let _ = connector.send_input(&InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y: 0,
        flags: pad as u32,
    });
}

fn button_bit(b: sdl3::gamepad::Button) -> Option<u32> {
    use sdl3::gamepad::Button;
    Some(match b {
        Button::South => wire::BTN_A,
        Button::East => wire::BTN_B,
        Button::West => wire::BTN_X,
        Button::North => wire::BTN_Y,
        Button::Back => wire::BTN_BACK,
        Button::Start => wire::BTN_START,
        Button::Guide => wire::BTN_GUIDE,
        Button::LeftStick => wire::BTN_LS_CLICK,
        Button::RightStick => wire::BTN_RS_CLICK,
        Button::LeftShoulder => wire::BTN_LB,
        Button::RightShoulder => wire::BTN_RB,
        Button::DPadUp => wire::BTN_DPAD_UP,
        Button::DPadDown => wire::BTN_DPAD_DOWN,
        Button::DPadLeft => wire::BTN_DPAD_LEFT,
        Button::DPadRight => wire::BTN_DPAD_RIGHT,
        Button::Touchpad => wire::BTN_TOUCHPAD,
        // PADDLE1/2/3/4 = R4/L4/R5/L5 (host `input::gamepad`).
        Button::RightPaddle1 => wire::BTN_PADDLE1,
        Button::LeftPaddle1 => wire::BTN_PADDLE2,
        Button::RightPaddle2 => wire::BTN_PADDLE3,
        Button::LeftPaddle2 => wire::BTN_PADDLE4,
        Button::Misc1 => wire::BTN_MISC1,
        _ => return None,
    })
}

/// The menu-navigation state of one open pad. Read off the handle, not from events — the
/// console polls, so a stick that never moves again still holds its direction.
fn menu_sample(pad: &sdl3::gamepad::Gamepad) -> MenuSample {
    use sdl3::gamepad::{Axis, Button};
    MenuSample {
        buttons: [
            pad.button(Button::South),
            pad.button(Button::East),
            pad.button(Button::West),
            pad.button(Button::North),
            pad.button(Button::LeftShoulder),
            pad.button(Button::RightShoulder),
        ],
        lx: pad.axis(Axis::LeftX),
        ly: pad.axis(Axis::LeftY),
        dpad: [
            pad.button(Button::DPadUp),
            pad.button(Button::DPadDown),
            pad.button(Button::DPadLeft),
            pad.button(Button::DPadRight),
        ],
    }
}

/// Fold every open pad into the one sample [`MenuNav`] steps: buttons and dpad OR'd, stick
/// from whoever is furthest off centre. Two pads pushing at once read as one hand instead of
/// cancelling, and one `MenuNav` keeps one repeat clock — per-pad ones race on the same list.
/// The Skia console already merges this way (`console/mod.rs`); this is the desktop half.
fn merge_samples(samples: &[MenuSample]) -> MenuSample {
    let mut out = MenuSample::default();
    let mut best = -1i64;
    for s in samples {
        for i in 0..out.buttons.len() {
            out.buttons[i] |= s.buttons[i];
        }
        for i in 0..out.dpad.len() {
            out.dpad[i] |= s.dpad[i];
        }
        // i64: two i16::MIN squares sum past i32::MAX.
        let mag = i64::from(s.lx).pow(2) + i64::from(s.ly).pow(2);
        if mag > best {
            best = mag;
            out.lx = s.lx;
            out.ly = s.ly;
        }
    }
    out
}

/// This pad is the one in someone's hands right now — a detent buzzes it, not its idle
/// neighbour. The stick test is the engage deadzone, so resting drift never claims the pulse.
fn is_acting(s: &MenuSample) -> bool {
    s.buttons.iter().chain(&s.dpad).any(|&b| b) || ring_sector(s.lx, s.ly, None).is_some()
}

/// The pad an input transition came from. `None` for hotplug and everything else.
fn input_pad(event: &sdl3::event::Event) -> Option<u32> {
    use sdl3::event::Event;
    match *event {
        Event::GamepadButtonDown { which, .. }
        | Event::GamepadButtonUp { which, .. }
        | Event::GamepadAxisMotion { which, .. }
        | Event::GamepadTouchpadDown { which, .. }
        | Event::GamepadTouchpadMotion { which, .. }
        | Event::GamepadTouchpadUp { which, .. }
        | Event::GamepadSensorUpdated { which, .. } => Some(which.raw()),
        _ => None,
    }
}

/// SDL sticks are +y = down; the wire (XInput) is +y = up. Triggers 0..32767 → 0..255.
fn axis_value(axis: sdl3::gamepad::Axis, v: i16) -> (u32, i32) {
    use sdl3::gamepad::Axis;
    match axis {
        Axis::LeftX => (wire::AXIS_LS_X, (v as i32).max(-32767)),
        Axis::LeftY => (wire::AXIS_LS_Y, -(v as i32).max(-32767)),
        Axis::RightX => (wire::AXIS_RS_X, (v as i32).max(-32767)),
        Axis::RightY => (wire::AXIS_RS_Y, -(v as i32).max(-32767)),
        Axis::TriggerLeft => (wire::AXIS_LT, (v as i32).clamp(0, 32767) >> 7),
        Axis::TriggerRight => (wire::AXIS_RT, (v as i32).clamp(0, 32767) >> 7),
    }
}

/// Decimal or `0x`-hex (DS5 report bytes are named in hex). `None` on a typo so it
/// falls back to the default rather than to zero.
fn env_u8(key: &str) -> Option<u8> {
    let v = std::env::var(key).ok()?;
    let v = v.trim();
    match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        Some(hex) => u8::from_str_radix(hex, 16).ok(),
        None => v.parse().ok(),
    }
}

/// One forwarded controller while a session is attached. Opening grabs the hidraw node
/// (SDL HIDAPI); idle/menu never populates slots.
struct Slot {
    /// SDL instance id (`ControllerDevice*::which`).
    id: u32,
    /// Wire pad index — lowest-free at open, stable for the slot's life.
    index: u8,
    pad: sdl3::gamepad::Gamepad,
    /// Physical kind, captured at open so feedback paths need no `&mut` SDL re-query.
    pref: GamepadPref,
    /// Kind declared in [`InputKind::GamepadArrival`] — the host's pretence, not `pref`.
    declared: GamepadPref,
    last_axis: [i32; 6],
    /// Held buttons, the Select chord and gesture, a local Guide: where this pad's input goes.
    route: PadRoute,
    /// Host-believed contacts `(surface, finger)`; lifted on close. 0 = legacy pad, 1/2 = Steam.
    held_touches: std::collections::HashSet<(u8, u8)>,
    /// Per Steam surface (0 = left, 1 = right): last wire coords + finger-down. Clicks have no
    /// position, so the click forward reuses the live contact.
    surface_last: [(i16, i16, bool); 2],
    /// Held Steam-pad clicks: motion frames would otherwise clear the bit host-side.
    held_clicks: [bool; 2],
    last_accel: [i16; 3],
    /// At least one motion sample went out — gates the zero-gyro park in [`Worker::flush_slot`].
    sent_motion: bool,
    /// Log-once: this path runs at the pad's sensor rate.
    motion_unreachable_logged: bool,
    /// bit0 = haptics, bit1 = speaker. Nonzero only for tier-A; bit0 also suppresses wire rumble
    /// while haptics frames arrive.
    audio_caps: u8,
    rumble_suppressed_logged: bool,
    /// A wire rumble went out since the coils were last armed; SDL's rumble bits mute them.
    coils_muted: bool,
    /// Raw passthrough for a Steam Controller declared as itself, either generation.
    sc2: Option<crate::sc2_capture::Sc2Capture>,
    /// The host sent raw writes, so its `0x80` reports own the motors and wire rumble is skipped.
    raw_rumble: bool,
}

impl Slot {
    fn new(
        id: u32,
        index: u8,
        pref: GamepadPref,
        declared: GamepadPref,
        pad: sdl3::gamepad::Gamepad,
    ) -> Slot {
        Slot {
            id,
            index,
            pad,
            pref,
            declared,
            last_axis: [i32::MIN; 6],
            route: PadRoute::default(),
            held_touches: std::collections::HashSet::new(),
            surface_last: [(0, 0, false); 2],
            held_clicks: [false; 2],
            last_accel: [0; 3],
            sent_motion: false,
            motion_unreachable_logged: false,
            audio_caps: 0,
            rumble_suppressed_logged: false,
            coils_muted: false,
            sc2: None,
            raw_rumble: false,
        }
    }

    /// Two touchpads: `TouchpadEx` surface encoding and pad-click re-route.
    fn is_multi_touchpad(&self) -> bool {
        self.pad.touchpads_count() >= 2
    }
}

/// The event a rumble update pushes to end the worker's wait; it carries nothing.
struct RumbleWake;

pub(super) struct Worker {
    subsystem: sdl3::GamepadSubsystem,
    /// Handed to each attached session: a rumble update ends the event wait at once.
    rumble_wake: Arc<dyn Fn() + Send + Sync>,
    pads_out: Arc<Mutex<Vec<PadInfo>>>,
    active_out: Arc<Mutex<Option<PadInfo>>>,
    /// Open only while a session is attached; opening grabs hardware.
    slots: Vec<Slot>,
    /// Menu pads while menu mode is on and no session; mutually exclusive with `slots`.
    /// EVERY connected pad, not the newest one: a second controller is otherwise dead on
    /// the console, and one left shut keeps the dark lightbar `reset_slot_feedback` gave it.
    menu_open: Vec<(u32, sdl3::gamepad::Gamepad)>,
    /// Menu pad that last had a button or stick engaged — the one a detent pulse belongs in.
    menu_last: Option<u32>,
    /// A forwarded slot or a menu pad just opened: menu navigation adopts until it has reported.
    menu_settle: MenuSettle,
    /// Menu pad power, `(id, level)`. Cached: [`publish`](Self::publish) runs on every hotplug.
    battery: Option<(u32, PadBattery)>,
    battery_at: Option<Instant>,
    /// Connected ids in connection order (metadata only, no open).
    order: Vec<u32>,
    /// Stable key; unmatched pin is kept (survives disconnect) and falls through to automatic.
    pinned: Option<String>,
    /// Off: [`Self::forwarded_ids`] is empty so a session opens no slot (hidraw stays free).
    forwarding: bool,
    /// Menu mode, the mask, the ring and the button policy: where input goes ([`route`]).
    gates: Gates,
    /// Applied at open to the kind DECLARED to the host, never to [`Slot::pref`].
    kind_override: GamepadPref,
    /// Owed synthetic presses and releases `(pad, bit, down, due)`, in send order. A tap's
    /// first press went out on receipt ([`tap_events`]).
    synthetic: Vec<(u8, u32, bool, Instant)>,
    /// bit0 = haptics, bit1 = speaker. `0` until declared: tier-A detection then never runs.
    pad_audio_prefs: u8,
    /// Off: wire rumble is drained and dropped.
    rumble: bool,
    attached: Option<Arc<NativeClient>>,
    escape_tx: async_channel::Sender<()>,
    disconnect_tx: async_channel::Sender<()>,
    /// Escape chord fully held — latched so it fires once.
    chord_armed: bool,
    chord_since: Option<Instant>,
    disconnect_fired: bool,
    menu_nav: MenuNav,
    menu_tx: async_channel::Sender<MenuEvent>,
    chord_tx: async_channel::Sender<(u8, SelectChord)>,
}

/// The attached session and the open slot SDL's `which` names. Takes the two fields,
/// not `&mut Worker`, so a caller still reads the worker's flags while holding the slot.
fn attached_slot<'a>(
    attached: &Option<Arc<NativeClient>>,
    slots: &'a mut [Slot],
    which: u32,
) -> Option<(Arc<NativeClient>, &'a mut Slot)> {
    let c = attached.clone()?;
    let slot = slots.iter_mut().find(|s| s.id == which)?;
    Some((c, slot))
}

impl Worker {
    /// The pinned pad, matched by stable key (most recent wins if two share one). An
    /// unmatched pin is `None` here and falls through to automatic without being cleared.
    fn pinned_id(&self) -> Option<u32> {
        let key = self.pinned.as_ref()?;
        self.order
            .iter()
            .rev()
            .copied()
            .find(|&id| self.pad_info(id).is_some_and(|p| &p.key == key))
    }

    /// The pin, else the most recently connected pad, but never Steam Input's virtual pad
    /// while a real one exists.
    fn active_id(&self) -> Option<u32> {
        self.pinned_id().or_else(|| {
            self.order
                .iter()
                .rev()
                .copied()
                .find(|&id| self.pad_info(id).is_some_and(|p| !p.steam_virtual))
                .or_else(|| self.order.last().copied())
        })
    }

    /// ID-based metadata — no device open (an open would grab the hardware).
    fn pad_info(&self, id: u32) -> Option<PadInfo> {
        if !self.order.contains(&id) {
            return None;
        }
        let jid = sdl3::joystick::JoystickId::new(id);
        let mut pref = pref_for_type(self.subsystem.type_for_id(jid));
        let (vid, pid) = (
            self.subsystem.vendor_for_id(jid).unwrap_or(0),
            self.subsystem.product_for_id(jid).unwrap_or(0),
        );
        if let Some(own) = pref_for_ids(vid, pid) {
            pref = own;
        }
        let name = self
            .subsystem
            .name_for_id(jid)
            .unwrap_or_else(|_| "Controller".into());
        let key = format!("{vid:04x}:{pid:04x}:{name}");
        let steam_virtual =
            (vid == 0x28DE && pid == 0x11FF) || name.starts_with("Steam Virtual Gamepad");
        // On a Deck, Steam Input's pad is the built-in controls until a session enables Valve
        // HIDAPI and the raw 28DE:1205 appears. The key keeps SDL's name so a pin still matches.
        let name = if steam_virtual && is_steam_deck() {
            pref = GamepadPref::SteamDeck;
            DECK_NAME.to_string()
        } else {
            name
        };
        Some(PadInfo {
            key,
            steam_virtual,
            name,
            pref,
            // SDL reports power only for an OPEN device; `publish` fills the one we hold.
            battery: None,
            // `forwarded`/`rumble` are console-screen fields; rumble, like battery, needs OPEN.
            detail: format!("{vid:04X}:{pid:04X}"),
            forwarded: true,
            rumble: false,
        })
    }

    /// Pin: only that pad. Automatic: every real pad, or every virtual one when that is all
    /// Steam Input exposes (Deck game-mode — else gyro/paddles have nowhere to land).
    fn forwarded_ids(&self) -> Vec<u32> {
        if !self.forwarding {
            return Vec::new();
        }
        if let Some(id) = self.pinned_id() {
            return vec![id];
        }
        let real: Vec<u32> = self
            .order
            .iter()
            .copied()
            .filter(|&id| self.pad_info(id).is_some_and(|p| !p.steam_virtual))
            .collect();
        if !real.is_empty() {
            return real;
        }
        // Every pad is virtual: Steam Input is wrapping all of them, so none is a shadow of
        // another and forwarding the lot double-counts nothing. Taking only the newest is
        // what left a Deck with two controllers seeing just one.
        self.order.clone()
    }

    /// What menu mode holds open: the pin, else every pad, Steam Input's virtual ones too.
    /// Navigation folds them into one sample, so a shadow beside its real pad costs nothing,
    /// and idle a Deck's built-in controls exist only as a virtual pad. Console navigation is
    /// local, so a user who turned wire forwarding off still drives the launcher. Steam's pad
    /// stands in for the raw Deck controller while both are listed ([`menu_pads`]).
    fn menu_ids(&self) -> Vec<u32> {
        let ids = self
            .pinned_id()
            .map_or_else(|| self.order.clone(), |id| vec![id]);
        menu_pads(ids, &self.order, |id| self.pad_info(id))
    }

    /// The one place that opens (= grabs) hardware: forwarded slots in a session, else the
    /// menu pads while menu mode or the ring wants them. Dropping a handle is
    /// `SDL_CloseGamepad`; on a Deck the firmware watchdog then restores lizard mode.
    fn sync_open(&mut self) {
        if self.attached.is_some() {
            self.menu_open.clear();
            self.menu_last = None;
            self.reconcile_slots();
            return;
        }
        self.close_all_slots();
        let want = if self.gates.input_route(false).opens_menu_pads() {
            self.menu_ids()
        } else {
            Vec::new()
        };
        let before = self.menu_open.len();
        self.menu_open.retain(|(id, _)| want.contains(id));
        let changed = self.menu_open.len() != before;
        let mut opened = false;
        for id in want {
            if self.menu_open.iter().any(|(open, _)| *open == id) {
                continue;
            }
            match self.subsystem.open(sdl3::joystick::JoystickId::new(id)) {
                Ok(pad) => {
                    self.menu_open.push((id, pad));
                    opened = true;
                }
                Err(e) => tracing::warn!(id, error = %e, "gamepad open failed"),
            }
        }
        // Hot-plug under the launcher adopts held state instead of firing it. A pad just opened
        // reads every button up until it reports, so it adopts through the settle. No sensors.
        if opened {
            self.menu_settle.start(Instant::now());
        } else if changed {
            self.menu_nav.reset();
        }
    }

    /// A controller-type change. In a session, each forwarded pad whose declared kind moves
    /// is closed and reopened, a re-plug the host builds the new kind from.
    fn set_kind_override(&mut self, pref: GamepadPref) {
        self.kind_override = pref;
        if self.attached.is_none() {
            return;
        }
        let moved: Vec<u32> = self
            .slots
            .iter()
            .filter(|s| declared_kind(pref, s.pref) != s.declared)
            .map(|s| s.id)
            .collect();
        for id in moved {
            if let Some(i) = self.slots.iter().position(|s| s.id == id) {
                self.close_slot_at(i);
                self.open_slot(id);
            }
        }
    }

    /// Close unwanted slots (flush first) and open newly-wanted pads into the lowest free
    /// index. A disconnect frees only its own index; others keep theirs.
    fn reconcile_slots(&mut self) {
        let want = self.forwarded_ids();
        let mut i = 0;
        while i < self.slots.len() {
            if want.contains(&self.slots[i].id) {
                i += 1;
            } else {
                self.close_slot_at(i);
            }
        }
        for id in want {
            if self.slots.iter().any(|s| s.id == id) {
                continue;
            }
            self.open_slot(id);
        }
    }

    fn open_slot(&mut self, id: u32) {
        let taken: Vec<u8> = self.slots.iter().map(|s| s.index).collect();
        let Some(index) = lowest_free_index(&taken) else {
            tracing::warn!(
                id,
                max = punktfunk_core::input::MAX_PADS,
                "gamepad slots full — controller not forwarded"
            );
            return;
        };
        let pref = self.pad_info(id).map_or(GamepadPref::Xbox360, |p| p.pref);
        let declared = declared_kind(self.kind_override, pref);
        match self.subsystem.open(sdl3::joystick::JoystickId::new(id)) {
            Ok(pad) => {
                let mut slot = Slot::new(id, index, pref, declared, pad);
                let raw = crate::sc2_capture::raw_model(
                    pref,
                    declared,
                    slot.pad.product_id().unwrap_or(0),
                );
                // A raw Steam Controller's IMU mode is Steam's to set, through the raw plane.
                if raw.is_none() {
                    Self::set_slot_sensors(&mut slot, true);
                }
                let (audio_caps, bluetooth) = self.pad_audio_caps_for(id, &slot.pad);
                slot.audio_caps = audio_caps;
                if let Some(path) = slot.pad.path() {
                    let (vid, pid) = (slot.pad.vendor_id(), slot.pad.product_id());
                    crate::sc2_capture::log_descriptor(&path, vid.unwrap_or(0), pid.unwrap_or(0));
                }
                // Kind before any input so the host builds a matching virtual device. Core
                // re-sends against datagram loss; an older host ignores it.
                if let Some(c) = &self.attached {
                    // Caps first — core ORs them into this arrival (and every re-send). Always
                    // set (0 for non-tier-A): wire indices are reused; leftover bits would stick.
                    c.set_pad_audio_caps(index, slot.audio_caps);
                    send(
                        c,
                        InputKind::GamepadArrival,
                        declared.to_u8() as u32,
                        0,
                        index,
                    );
                    // Always set (defaults for a well-behaved pad): wire indices are reused.
                    let quirks = if pref == GamepadPref::SteamDeck {
                        ActuatorQuirks {
                            keepalive_ms: DECK_RUMBLE_KEEPALIVE_MS,
                            min_pulse_ms: 0,
                            dedup_jitter: true,
                        }
                    } else {
                        ActuatorQuirks::default()
                    };
                    c.set_rumble_quirks(index as u16, quirks);
                    // After the arrival, so the host builds the as-is pad before raw reports.
                    if let Some(model) = raw {
                        slot.sc2 = slot.pad.path().and_then(|path| {
                            c.set_sc2_gate(index, self.gates.sc2());
                            crate::sc2_capture::Sc2Capture::open(
                                &path,
                                c.clone(),
                                index,
                                slot.pad.serial_number(),
                                model,
                            )
                        });
                    }
                }
                if slot.audio_caps != 0 {
                    if slot.audio_caps & 0x01 != 0 {
                        // SDL rumble sets ucEnableBits1 0x01|0x02, muting the 0xD1 coils.
                        // Clear those bits at open; render_feedback clears them again when
                        // haptics resume after a rumble. Fails if hid-playstation owns the HID
                        // link — that driver asserts the same bit on every FF update.
                        if let Err(e) = slot.pad.send_effect(&Ds5Feedback::audio_haptics_packet()) {
                            tracing::info!(
                                index,
                                error = %e,
                                "could not re-arm the DualSense's audio-haptics bit (SDL does \
                                 not own this pad's HID link) — haptics still work unless \
                                 something else has rumbled the pad this plug-in"
                            );
                        }
                    }
                    if slot.audio_caps & 0x02 != 0 {
                        // Channel 1 powers up on the headphone jack; see `speaker_enable_packet`.
                        let path = env_u8("PUNKTFUNK_PAD_SPEAKER_PATH").unwrap_or(0x20);
                        let volume = env_u8("PUNKTFUNK_PAD_SPEAKER_VOLUME").unwrap_or(0x64);
                        if let Err(e) = slot
                            .pad
                            .send_effect(&Ds5Feedback::speaker_enable_packet(volume, path))
                        {
                            tracing::info!(
                                index,
                                error = %e,
                                "could not point the DualSense at its own speaker (SDL does \
                                 not own this pad's HID link) — the pad's speaker may stay \
                                 silent even though the stream reaches it"
                            );
                        }
                    }
                    crate::pad_audio::register_tier_a(index, slot.pad.path(), bluetooth);
                    tracing::info!(
                        index,
                        caps = slot.audio_caps,
                        bluetooth,
                        "tier-A DualSense: pad-audio render caps declared"
                    );
                }
                tracing::info!(
                    id,
                    index,
                    pref = ?pref,
                    declared = ?declared,
                    raw = slot.sc2.is_some(),
                    "gamepad forwarding (slot opened)"
                );
                self.slots.push(slot);
                self.menu_settle.start(Instant::now());
            }
            Err(e) => tracing::warn!(id, error = %e, "gamepad open failed"),
        }
    }

    /// Settings prefs for a physical DualSense/Edge (VID:PID, never the declared kind), and
    /// whether it is on Bluetooth; 0 otherwise. The link is SDL's; Unknown counts as wired only
    /// with a 4-ch audio sibling. Bluetooth audio needs a raw HID handle beside SDL's.
    fn pad_audio_caps_for(&self, id: u32, pad: &sdl3::gamepad::Gamepad) -> (u8, bool) {
        if self.pad_audio_prefs == 0 {
            return (0, false);
        }
        let jid = sdl3::joystick::JoystickId::new(id);
        let vid = self.subsystem.vendor_for_id(jid).unwrap_or(0);
        let pid = self.subsystem.product_for_id(jid).unwrap_or(0);
        if !crate::pad_audio::is_tier_a_ds5(vid, pid) {
            return (0, false);
        }
        use sdl3::joystick::ConnectionState;
        let path = pad.path();
        let bluetooth = match pad.connection_state() {
            Ok(ConnectionState::Wired) => false,
            Ok(ConnectionState::Wireless) => true,
            _ if crate::pad_audio::wired_audio_sibling(path.as_deref()) => false,
            _ => return (0, false),
        };
        if bluetooth && path.is_none_or(|p| crate::sc2_capture::Dev::open(&p).is_none()) {
            tracing::info!(
                id,
                "bluetooth DualSense HID node did not open — pad audio off"
            );
            return (0, false);
        }
        (self.pad_audio_prefs, bluetooth)
    }

    /// Flush held wire state and drop the SDL handle. Flush is wire-only, so unplug is safe.
    fn close_slot_at(&mut self, i: usize) {
        // Raw reports stop first: one landing after the remove would outlive the slot.
        self.slots[i].sc2 = None;
        // Silence before the handle drops; do not depend on SDL at close. Errors if already gone.
        let _ = self.slots[i].pad.set_rumble(0, 0, 100);
        Self::reset_slot_feedback(&mut self.slots[i]);
        if let Some(c) = self.attached.clone() {
            Self::flush_slot(&c, &mut self.slots[i]);
            // After the flush so seq is past the zeroing snapshots; host seq-gates resurrection.
            send(&c, InputKind::GamepadRemove, 0, 0, self.slots[i].index);
        }
        let slot = self.slots.remove(i);
        if slot.audio_caps != 0 {
            crate::pad_audio::unregister_tier_a(slot.index);
            crate::pad_audio::clear_haptics_liveness(slot.index);
            crate::pad_audio::note_rumble(slot.index, 0, 0, 0);
        }
        tracing::info!(
            id = slot.id,
            index = slot.index,
            "gamepad forwarding stopped (slot closed)"
        );
    }

    /// Neutral the physical pad before the handle closes. Rumble decays; adaptive-trigger
    /// and lightbar are latched in firmware and survive the stream. Best-effort: pad may
    /// already be gone.
    fn reset_slot_feedback(slot: &mut Slot) {
        if matches!(
            slot.pref,
            GamepadPref::DualSense | GamepadPref::DualSenseEdge
        ) {
            // Mode 0x00 = no effect. Both sides, then lightbar dark and player LEDs clear.
            for which in [0u8, 1] {
                let _ = slot
                    .pad
                    .send_effect(&Ds5Feedback::trigger_packet(which, &[0u8; 11]));
            }
            let _ = slot.pad.send_effect(&Ds5Feedback::lightbar_packet(0, 0, 0));
            let _ = slot.pad.send_effect(&Ds5Feedback::player_packet(0));
        } else {
            let _ = slot.pad.set_led(0, 0, 0);
        }
    }

    pub(super) fn close_all_slots(&mut self) {
        while !self.slots.is_empty() {
            self.close_slot_at(0);
        }
    }

    fn push_sc2_gate(&self) {
        let Some(c) = &self.attached else { return };
        let gate = self.gates.sc2();
        for slot in self.slots.iter().filter(|s| s.sc2.is_some()) {
            c.set_sc2_gate(slot.index, gate);
        }
    }

    /// Motion sensors stream only while a session wants them (USB/BT bandwidth). Once at open.
    fn set_slot_sensors(slot: &mut Slot, enabled: bool) {
        use sdl3::sensor::SensorType;
        for s in [SensorType::Gyroscope, SensorType::Accelerometer] {
            // SAFETY: an SDL3 query on the gamepad this slot owns and keeps open; it takes a
            // plain sensor-type enum and only reads device state.
            if unsafe { slot.pad.has_sensor(s) } {
                let _ = slot.pad.sensor_set_enabled(s, enabled);
            }
        }
    }

    /// The ring or a launch hold. Before attach it reads the menu pads, so they open even
    /// outside menu mode.
    fn set_ring_nav(&mut self, on: bool) {
        self.gates.ring_nav = on;
        self.menu_nav.reset();
        if self.attached.is_none() {
            self.sync_open();
        }
    }

    /// After hotplug or pin change. A pad holding the escape chord may have just unplugged.
    fn refresh_active(&mut self) {
        self.sync_open();
        self.rearm_escape();
        self.publish();
    }

    /// Zero host-held state. Wire events only — safe against an already-removed pad.
    fn flush_slot(c: &NativeClient, slot: &mut Slot) {
        let pad = slot.index;
        for (b, down) in slot.route.release() {
            send(c, InputKind::GamepadButton, b, down as i32, pad);
        }
        for (id, v) in slot.last_axis.iter_mut().enumerate() {
            if *v != 0 && *v != i32::MIN {
                send(c, InputKind::GamepadAxis, id as u32, 0, pad);
            }
            *v = i32::MIN;
        }
        for i in 0..2usize {
            if std::mem::take(&mut slot.held_clicks[i]) {
                let (x, y, _) = slot.surface_last[i];
                let _ = c.send_rich_input(RichInput::TouchpadEx {
                    pad,
                    surface: (i as u8) + 1,
                    finger: 0,
                    touch: false,
                    click: false,
                    x,
                    y,
                    pressure: 0,
                });
            }
        }
        slot.surface_last = [(0, 0, false); 2];
        for (surface, finger) in slot.held_touches.drain() {
            let rich = if surface == 0 {
                RichInput::Touchpad {
                    pad,
                    finger,
                    active: false,
                    x: 0,
                    y: 0,
                }
            } else {
                RichInput::TouchpadEx {
                    pad,
                    surface,
                    finger,
                    touch: false,
                    click: false,
                    x: 0,
                    y: 0,
                    pressure: 0,
                }
            };
            let _ = c.send_rich_input(rich);
        }
        // Gyro is level-triggered host-side; a close mid-rotation leaves the virtual pad turning.
        // Keep accel: gravity does not stop with the session.
        if std::mem::take(&mut slot.sent_motion) {
            let _ = c.send_rich_input(RichInput::Motion {
                pad,
                gyro: [0; 3],
                accel: slot.last_accel,
            });
        }
    }

    /// Overlay mask lifts: every slot takes its pad back.
    fn readopt_held(&mut self) {
        let attached = self.attached.clone();
        for slot in &mut self.slots {
            Self::readopt_slot(slot, attached.as_deref(), self.gates.system_forward);
        }
        self.rearm_escape();
    }

    /// The pad is ours again: [`PadRoute::adopt`] takes what is down now, without a wire
    /// press. Axes are re-sent: the flush zeroed them and SDL only speaks on change, so a
    /// still-held stick would stay dead host-side. A local Guide still down keeps the pad.
    fn readopt_slot(slot: &mut Slot, c: Option<&NativeClient>, system_forward: bool) {
        use sdl3::gamepad::{Axis, Button};
        const BUTTONS: [Button; 21] = [
            Button::South,
            Button::East,
            Button::West,
            Button::North,
            Button::Back,
            Button::Start,
            Button::Guide,
            Button::LeftStick,
            Button::RightStick,
            Button::LeftShoulder,
            Button::RightShoulder,
            Button::DPadUp,
            Button::DPadDown,
            Button::DPadLeft,
            Button::DPadRight,
            Button::Touchpad,
            Button::RightPaddle1,
            Button::LeftPaddle1,
            Button::RightPaddle2,
            Button::LeftPaddle2,
            Button::Misc1,
        ];
        const AXES: [Axis; 6] = [
            Axis::LeftX,
            Axis::LeftY,
            Axis::RightX,
            Axis::RightY,
            Axis::TriggerLeft,
            Axis::TriggerRight,
        ];
        // A trackpad click forwards as a surface, and its release never clears a bit.
        let down: Vec<u32> = BUTTONS
            .into_iter()
            .filter(|&b| Self::steam_click_surface(slot, b).is_none() && slot.pad.button(b))
            .filter_map(button_bit)
            .collect();
        let guide = slot.pad.button(Button::Guide);
        if !slot.route.adopt(down, guide, system_forward) {
            return;
        }
        let Some(c) = c else {
            return;
        };
        for a in AXES {
            let (id, v) = axis_value(a, slot.pad.axis(a));
            if slot.last_axis[id as usize] != v {
                slot.last_axis[id as usize] = v;
                send(c, InputKind::GamepadAxis, id, v, slot.index);
            }
        }
    }

    /// Read from the pads, not `held_buttons`: the mask drops button events, and a hold
    /// that began before it must still complete or let go.
    fn chord_held(&self) -> bool {
        self.slots
            .iter()
            .any(|s| ESCAPE_CHORD.iter().all(|&b| s.pad.button(b)))
    }

    fn maybe_fire_escape(&mut self) {
        if self.chord_armed {
            return;
        }
        if self.chord_held() {
            self.chord_armed = true;
            self.chord_since = Some(Instant::now());
            let _ = self.escape_tx.try_send(());
            tracing::info!(
                "gamepad escape chord (L1+R1+Start+Select) — leaving fullscreen (hold to disconnect)"
            );
        }
    }

    /// Hold threshold and a tap's owed presses and releases. ~10 ms attached jitter at most.
    pub(super) fn gesture_poll(&mut self) {
        let Some(c) = self.attached.clone() else {
            self.synthetic.clear();
            return;
        };
        let now = Instant::now();
        self.synthetic.retain(|&(pad, bit, down, due)| {
            if now >= due {
                send(&c, InputKind::GamepadButton, bit, down as i32, pad);
                false
            } else {
                true
            }
        });
        if !self.gates.guide_gesture {
            return;
        }
        for slot in &mut self.slots {
            let mut due = Vec::new();
            slot.route.gesture.poll(now, &mut due);
            for (b, down) in due {
                send(&c, InputKind::GamepadButton, b, down as i32, slot.index);
            }
        }
    }

    /// Polled so the hold completes, or lets go, without new events — a mask drops them.
    pub(super) fn maybe_fire_disconnect(&mut self) {
        self.rearm_escape();
        if self.disconnect_fired {
            return;
        }
        if let Some(since) = self.chord_since {
            if since.elapsed() >= DISCONNECT_HOLD {
                self.disconnect_fired = true;
                let _ = self.disconnect_tx.try_send(());
                tracing::info!("gamepad escape chord held — disconnecting");
            }
        }
    }

    fn rearm_escape(&mut self) {
        if self.chord_armed && !self.chord_held() {
            self.reset_chord();
        }
    }

    /// Session boundary: hold-to-disconnect ends the session while the chord is still held,
    /// so button-ups arrive after detach and `rearm_escape` never runs. Without this the
    /// latch leaks into the next session (swallows the first chord or fires a stale disconnect).
    fn reset_chord(&mut self) {
        self.chord_armed = false;
        self.chord_since = None;
        self.disconnect_fired = false;
    }

    /// Steam pads: `TouchpadEx` (SDL 0 = left → surface 1, signed). DualSense: legacy unsigned.
    fn forward_touch(
        c: &NativeClient,
        slot: &mut Slot,
        touchpad: u32,
        finger: u8,
        x: f32,
        y: f32,
        active: bool,
    ) {
        let pad = slot.index;
        let multi = slot.is_multi_touchpad();
        let (cx, cy) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
        let surface = if multi { (touchpad as u8) + 1 } else { 0 };
        let rich = if multi {
            let (wx, wy) = (
                (cx * 65535.0 - 32768.0) as i16,
                (cy * 65535.0 - 32768.0) as i16,
            );
            let i = (surface - 1).min(1) as usize;
            slot.surface_last[i] = (wx, wy, active);
            RichInput::TouchpadEx {
                pad,
                surface,
                finger,
                touch: active,
                // Click is a separate button event; carry held so motion cannot clear it.
                click: slot.held_clicks[i],
                x: wx,
                y: wy,
                pressure: 0,
            }
        } else {
            RichInput::Touchpad {
                pad,
                finger,
                active,
                x: (cx * 65535.0) as u16,
                y: (cy * 65535.0) as u16,
            }
        };
        let _ = c.send_rich_input(rich);
        if active {
            slot.held_touches.insert((surface, finger));
        } else {
            slot.held_touches.remove(&(surface, finger));
        }
    }

    /// Steam pad clicks arrive as buttons (`touchpad` = left, `misc2` = right). Must not
    /// ride the button plane: the host maps `BTN_TOUCHPAD` to the RIGHT pad. DualSense's
    /// single touchpad button stays a wire button.
    fn steam_click_surface(slot: &Slot, button: sdl3::gamepad::Button) -> Option<u8> {
        use sdl3::gamepad::Button;
        if !slot.is_multi_touchpad() {
            return None;
        }
        match button {
            Button::Touchpad => Some(1),
            Button::Misc2 => Some(2),
            _ => None,
        }
    }

    /// Clicks carry no position — reuse the live contact. `touch` stays asserted while
    /// down even if the touch event has not arrived yet.
    fn forward_click(c: &NativeClient, slot: &mut Slot, surface: u8, down: bool) {
        let i = (surface - 1).min(1) as usize;
        slot.held_clicks[i] = down;
        let (x, y, touching) = slot.surface_last[i];
        let _ = c.send_rich_input(RichInput::TouchpadEx {
            pad: slot.index,
            surface,
            finger: 0,
            touch: touching || down,
            click: down,
            x,
            y,
            pressure: 0,
        });
    }

    fn publish(&self) {
        // `pad_info` is open-free; SDL reports power only for an OPEN device. Other pads
        // stay `None` — we cannot know without grabbing hardware that is not ours.
        let with_battery = |id: u32| -> Option<PadInfo> {
            let mut info = self.pad_info(id)?;
            if let Some((bid, b)) = self.battery {
                if bid == id {
                    info.battery = Some(b);
                }
            }
            Some(info)
        };
        let mut list: Vec<PadInfo> = self
            .order
            .iter()
            .copied()
            .filter_map(with_battery)
            .collect();
        if is_steam_deck() {
            fold_deck_shadows(&mut list);
        }
        list.reverse();
        *self.pads_out.lock().unwrap() = list;
        *self.active_out.lock().unwrap() = self.active_id().and_then(with_battery);
    }

    /// Polled: nothing reports a battery changing. Menu pads only (the ones the console holds).
    pub(super) fn battery_poll(&mut self) {
        // The UI shows one level, for the pad it calls active; with several open that is the
        // only one worth a poll (`publish` matches it back by id).
        let active = self.active_id();
        let Some((id, pad)) = self
            .menu_open
            .iter()
            .find(|(id, _)| Some(*id) == active)
            .or_else(|| self.menu_open.first())
        else {
            if self.battery.take().is_some() {
                self.publish();
            }
            self.battery_at = None;
            return;
        };
        let now = Instant::now();
        if self
            .battery_at
            .is_some_and(|t| now.duration_since(t) < BATTERY_POLL)
        {
            return;
        }
        self.battery_at = Some(now);
        let fresh = battery_of(pad).map(|b| (*id, b));
        if fresh != self.battery {
            self.battery = fresh;
            self.publish();
        }
    }

    /// False when the app side is gone and the worker should exit.
    pub(super) fn drain_ctl(&mut self, ctl: &Receiver<Ctl>) -> bool {
        loop {
            match ctl.try_recv() {
                Ok(Ctl::Attach(c)) => {
                    c.set_rumble_wake(self.rumble_wake.clone());
                    self.attached = Some(c);
                    self.reset_chord();

                    // The Deck driver only in-session. Not with forwarding off: its enumeration
                    // kills the trackpad-mouse and grabs hardware a passthrough needs.
                    if self.forwarding {
                        set_valve_hidapi(true);
                    }
                    self.sync_open();
                    log_joysticks(&self.subsystem);
                }
                Ok(Ctl::Detach) => {
                    self.close_all_slots();
                    self.attached = None;
                    self.reset_chord();
                    // Driver off before the menu pads reopen: no slot is left to outlive it.
                    set_valve_hidapi(false);
                    self.sync_open();
                    if self.gates.menu_mode {
                        // Adopt still-held buttons so the escape chord cannot ghost-fire the menu.
                        self.menu_nav.reset();
                    }
                }
                Ok(Ctl::Pin(key)) => {
                    self.pinned = key;
                    self.refresh_active();
                }
                Ok(Ctl::KindOverride(pref)) => self.set_kind_override(pref),
                Ok(Ctl::ChordsLive(on)) => {
                    self.gates.chords_live = on;
                    self.push_sc2_gate();
                }
                Ok(Ctl::SystemButtons {
                    forward_raw,
                    gesture,
                }) => {
                    self.gates.system_forward = forward_raw;
                    self.push_sc2_gate();
                    if self.gates.guide_gesture == gesture {
                        continue;
                    }
                    self.gates.guide_gesture = gesture;
                    // Mid-session flip can strand a synthetic Guide or owed tap; lift now.
                    if let Some(c) = self.attached.clone() {
                        for slot in &mut self.slots {
                            let mut due = Vec::new();
                            slot.route.gesture.flush(&mut due);
                            for (b, down) in due {
                                send(&c, InputKind::GamepadButton, b, down as i32, slot.index);
                            }
                        }
                    }
                }
                Ok(Ctl::TapButton(bit)) => {
                    // On the first forwarded index (pad 0 if none). A synthetic `MISC1` is
                    // the Quick Access action, pressed as that pad's host device reads it.
                    if let Some(c) = self.attached.clone() {
                        let slot = self.slots.first();
                        let pad = slot.map_or(0, |s| s.index);
                        let presses: &[u32] = if bit == wire::BTN_MISC1 {
                            wire::quick_access_presses(
                                punktfunk_core::config::pad_has_quick_access(
                                    slot.map_or(c.requested_gamepad, |s| s.declared),
                                    c.requested_gamepad,
                                    c.resolved_gamepad,
                                ),
                            )
                        } else {
                            &[bit]
                        };
                        let now = Instant::now();
                        for (bit, down, after) in tap_events(presses) {
                            if after.is_zero() {
                                send(&c, InputKind::GamepadButton, bit, down as i32, pad);
                            } else {
                                self.synthetic.push((pad, bit, down, now + after));
                            }
                        }
                    }
                }
                Ok(Ctl::Mask(on)) => {
                    if self.gates.masked == on {
                        continue;
                    }
                    self.gates.masked = on;
                    self.push_sc2_gate();
                    if on {
                        // Neutral now, slots stay open — the host must not see an unplug.
                        // An escape hold carries on: the escape itself raises this mask.
                        if let Some(c) = self.attached.clone() {
                            for slot in &mut self.slots {
                                Self::flush_slot(&c, slot);
                            }
                        }
                    } else {
                        self.readopt_held();
                        self.menu_nav.reset();
                    }
                    tracing::info!(masked = on, "overlay input mask");
                }
                Ok(Ctl::Forwarding(on)) => {
                    if self.forwarding == on {
                        continue;
                    }
                    self.forwarding = on;
                    self.reset_chord();

                    // ON: enable the Deck driver before `sync_open` or a Deck pad opens under
                    // its old identity. OFF: disable after, so no slot outlives the driver.
                    let attached = self.attached.is_some();
                    if on && attached {
                        set_valve_hidapi(true);
                    }
                    self.sync_open();
                    if !on && attached {
                        set_valve_hidapi(false);
                    }
                }
                Ok(Ctl::PadAudioPrefs(bits)) => self.pad_audio_prefs = bits & 0x03,
                Ok(Ctl::Rumble(on)) => self.rumble = on,
                Ok(Ctl::MenuMode(on)) => {
                    self.gates.menu_mode = on;
                    if on {
                        self.menu_nav.reset();
                    }
                    self.sync_open();
                }
                Ok(Ctl::RingNav(on)) => self.set_ring_nav(on),
                Ok(Ctl::MenuRumble(pulse)) => {
                    if self.attached.is_none() {
                        // The pad that last acted, not the newest: with two on the console the
                        // detent belongs in the hands that moved the cursor.
                        let i = self
                            .menu_open
                            .iter()
                            .position(|(id, _)| Some(*id) == self.menu_last)
                            .unwrap_or(0);
                        if let Some((_, pad)) = self.menu_open.get_mut(i) {
                            let (low, high, ms) = match pulse {
                                MenuPulse::Move => (0, 0x3000, 25),
                                MenuPulse::Confirm => (0x5000, 0x5000, 60),
                                MenuPulse::Boundary => (0x6000, 0, 60),
                            };
                            let _ = pad.set_rumble(low, high, ms);
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return false,
            }
        }
    }

    /// Hotplug always counts, or the slot table goes stale. Every input transition goes
    /// through [`route`].
    pub(super) fn handle_event(&mut self, event: sdl3::event::Event) {
        use sdl3::event::Event;
        match event {
            Event::GamepadAdded { which, .. } => {
                let which = which.raw();
                if !self.order.contains(&which) {
                    self.order.push(which);
                    if let Some(p) = self.pad_info(which) {
                        tracing::info!(
                            name = p.name,
                            key = p.key,
                            pref = ?p.pref,
                            steam_virtual = p.steam_virtual,
                            "gamepad attached"
                        );
                    }
                    self.refresh_active();
                }
            }
            Event::GamepadRemoved { which, .. } => {
                let which = which.raw();
                if self.order.contains(&which) {
                    self.order.retain(|&id| id != which);
                    tracing::info!("gamepad detached");
                    self.refresh_active();
                }
            }
            _ => self.on_input(&event),
        }
    }

    /// One transition on a forwarded pad: [`route`] decides, this performs. The escape
    /// chord is checked last, once the slot borrow has ended.
    fn on_input(&mut self, event: &sdl3::event::Event) {
        let Some(which) = input_pad(event) else {
            return;
        };
        let Some((c, slot)) = attached_slot(&self.attached, &mut self.slots, which) else {
            return;
        };
        let Some(input) = Self::pad_input(slot, event) else {
            return;
        };
        let mut escape = None;
        for action in route(input, &self.gates, &mut slot.route, Instant::now()) {
            match action {
                Action::Wire { bit, down } => {
                    send(&c, InputKind::GamepadButton, bit, down as i32, slot.index)
                }
                Action::Click { surface, down } => Self::forward_click(&c, slot, surface, down),
                Action::Forward => Self::forward(&c, slot, event),
                Action::Flush => Self::flush_slot(&c, slot),
                Action::Readopt => Self::readopt_slot(slot, Some(&*c), self.gates.system_forward),
                Action::Chord(chord) => {
                    let _ = self.chord_tx.try_send((slot.index, chord));
                }
                Action::Escape | Action::Rearm => escape = Some(action),
            }
        }
        match escape {
            Some(Action::Escape) => self.maybe_fire_escape(),
            Some(Action::Rearm) => self.rearm_escape(),
            _ => {}
        }
    }

    /// A button as a wire bit, or a Steam pad's trackpad click. `None` for a button with no
    /// wire bit.
    fn pad_input(slot: &Slot, event: &sdl3::event::Event) -> Option<PadInput> {
        use sdl3::event::Event;
        let button = |button, down| match Self::steam_click_surface(slot, button) {
            Some(surface) => Some(PadInput::Click { surface, down }),
            None => button_bit(button).map(|bit| PadInput::Button { bit, down }),
        };
        match *event {
            Event::GamepadButtonDown { button: b, .. } => button(b, true),
            Event::GamepadButtonUp { button: b, .. } => button(b, false),
            Event::GamepadAxisMotion { .. }
            | Event::GamepadTouchpadDown { .. }
            | Event::GamepadTouchpadMotion { .. }
            | Event::GamepadTouchpadUp { .. }
            | Event::GamepadSensorUpdated { .. } => Some(PadInput::Analog),
            _ => None,
        }
    }

    /// An analog sample [`route`] let through: an axis on change, touch and motion as is.
    fn forward(c: &NativeClient, slot: &mut Slot, event: &sdl3::event::Event) {
        use sdl3::event::Event;
        match *event {
            Event::GamepadAxisMotion { axis, value, .. } => {
                let (id, v) = axis_value(axis, value);
                if slot.last_axis[id as usize] != v {
                    slot.last_axis[id as usize] = v;
                    send(c, InputKind::GamepadAxis, id, v, slot.index);
                }
            }
            Event::GamepadTouchpadDown {
                touchpad,
                finger,
                x,
                y,
                ..
            }
            | Event::GamepadTouchpadMotion {
                touchpad,
                finger,
                x,
                y,
                ..
            } => Self::forward_touch(c, slot, touchpad as u32, finger as u8, x, y, true),
            Event::GamepadTouchpadUp {
                touchpad,
                finger,
                x,
                y,
                ..
            } => Self::forward_touch(c, slot, touchpad as u32, finger as u8, x, y, false),
            Event::GamepadSensorUpdated { sensor, data, .. } => {
                Self::forward_motion(c, slot, sensor, data)
            }
            _ => {}
        }
    }

    fn forward_motion(
        c: &NativeClient,
        slot: &mut Slot,
        sensor: sdl3::sensor::SensorType,
        data: [f32; 3],
    ) {
        use sdl3::sensor::SensorType;
        match sensor {
            SensorType::Accelerometer => {
                for (i, v) in data.iter().enumerate() {
                    slot.last_accel[i] = (v / G * ACCEL_LSB_PER_G).clamp(-32768.0, 32767.0) as i16;
                }
            }
            SensorType::Gyroscope => {
                // Per-pad declaration, not the session echo: under Auto the Hello
                // carries pad 0's kind while pad 1 may still have a gyro.
                if !punktfunk_core::config::pad_motion_reaches(
                    slot.declared,
                    c.requested_gamepad,
                    c.resolved_gamepad,
                ) {
                    if !slot.motion_unreachable_logged {
                        slot.motion_unreachable_logged = true;
                        tracing::warn!(
                            pad = slot.index,
                            declared = ?slot.declared,
                            resolved = ?c.resolved_gamepad,
                            "this controller has a gyro but the host built it a backend \
                             without one — motion will not reach the game; pick a \
                             DualSense-class controller type to get it"
                        );
                    }
                    return;
                }
                let mut gyro = [0i16; 3];
                for (i, v) in data.iter().enumerate() {
                    gyro[i] = (v * GYRO_LSB_PER_RAD_S).clamp(-32768.0, 32767.0) as i16;
                }
                slot.sent_motion = true;
                let _ = c.send_rich_input(RichInput::Motion {
                    pad: slot.index,
                    gyro,
                    accel: slot.last_accel,
                });
            }
            _ => {}
        }
    }

    /// The ring reads every forwarded pad: it opens from whichever pad pressed Select+A, which
    /// is not always slot 0. A masked menu reads none, so one stick never scrolls Steam's UI
    /// and ours.
    pub(super) fn menu_poll(&mut self) {
        let reads = self.gates.input_route(self.attached.is_some()).menu_reads();
        let pads: Vec<(u32, &sdl3::gamepad::Gamepad)> = match reads {
            MenuReads::Slots => self.slots.iter().map(|s| (s.id, &s.pad)).collect(),
            MenuReads::MenuPads => self.menu_open.iter().map(|(id, p)| (*id, p)).collect(),
            MenuReads::Nothing => return,
        };
        if pads.is_empty() {
            return;
        }
        let samples: Vec<MenuSample> = pads.iter().map(|(_, p)| menu_sample(p)).collect();
        // Latest connected wins a tie, matching the active-pad rule everywhere else.
        if let Some(i) = samples.iter().rposition(is_acting) {
            self.menu_last = Some(pads[i].0);
        }
        let merged = merge_samples(&samples);
        let now = Instant::now();
        if self.menu_settle.adopting(now) {
            self.menu_nav.reset();
        }
        let mut out = Vec::new();
        self.menu_nav.poll(&merged, now, &mut out);
        for e in out {
            let _ = self.menu_tx.try_send(e);
        }
    }

    /// Apply one engine command verbatim. `backstop_ms` is the SDL duration — a hardware
    /// net under a stalled worker; the engine emits explicit zeros at every policy stop.
    /// A tier-A pad's level is mirrored so its Bluetooth media reports carry it.
    fn issue_rumble(slot: &mut Slot, low: u16, high: u16, backstop_ms: u32) {
        let dur_ms: u32 = if (low, high) == (0, 0) {
            100
        } else {
            // No local floor: actuator floors live in `ActuatorQuirks::min_pulse_ms`.
            backstop_ms
        };
        if slot.audio_caps != 0 {
            crate::pad_audio::note_rumble(slot.index, low, high, dur_ms);
        }
        match slot.pad.set_rumble(low, high, dur_ms) {
            Err(e) => {
                tracing::warn!(pad = slot.index, low, high, error = %e, "rumble: SDL set_rumble failed")
            }
            Ok(()) => tracing::trace!(pad = slot.index, low, high, "rumble: rendered"),
        }
    }

    /// Single consumer of rumble + HID output. Engine commands are already effective;
    /// this worker applies them verbatim, or drops them all with rumble off. A raw SC2 hands
    /// its motors to the host's raw writes. A tier-A pad's coils go to whichever of wire
    /// rumble and haptics audio is arriving.
    pub(super) fn render_feedback(&mut self) {
        let Some(connector) = self.attached.clone() else {
            return;
        };
        // Haptics resumed after a rumble muted the coils: arm them again.
        for slot in &mut self.slots {
            if slot.coils_muted && crate::pad_audio::haptics_live(slot.index) {
                slot.coils_muted = false;
                if let Err(e) = slot.pad.send_effect(&Ds5Feedback::audio_haptics_packet()) {
                    tracing::debug!(pad = slot.index, error = %e, "audio-haptics re-arm");
                }
            }
        }
        while let Ok(cmd) = connector.next_rumble_command(Duration::ZERO) {
            if !self.rumble {
                continue;
            }
            if let Some(slot) = self.slots.iter_mut().find(|s| s.index as u16 == cmd.pad) {
                if slot.raw_rumble {
                    continue;
                }
                // SDL rumble sets ucEnableBits1 0x01|0x02, muting the 0xD1 coils. A title that
                // renders no haptics audio sends no frames, so it keeps its rumble.
                let haptics = slot.audio_caps & 0x01 != 0;
                if haptics && crate::pad_audio::haptics_live(slot.index) {
                    if !slot.rumble_suppressed_logged {
                        slot.rumble_suppressed_logged = true;
                        tracing::info!(
                            pad = slot.index,
                            "wire rumble suppressed — the pad-audio haptics stream carries feedback"
                        );
                    }
                    continue;
                }
                slot.coils_muted |= haptics;
                Self::issue_rumble(slot, cmd.low, cmd.high, cmd.backstop_ms);
            }
        }
        while let Ok(hid) = connector.next_hidout(Duration::ZERO) {
            let idx = hidout_pad(&hid);
            let Some(slot) = self.slots.iter_mut().find(|s| s.index == idx) else {
                continue;
            };
            let is_ds = matches!(
                slot.pref,
                GamepadPref::DualSense | GamepadPref::DualSenseEdge
            );
            match hid {
                HidOutput::Led { r, g, b, .. } if is_ds => {
                    let _ = slot.pad.send_effect(&Ds5Feedback::lightbar_packet(r, g, b));
                }
                HidOutput::Led { r, g, b, .. } => {
                    let _ = slot.pad.set_led(r, g, b);
                }
                HidOutput::PlayerLeds { bits, .. } if is_ds => {
                    let _ = slot.pad.send_effect(&Ds5Feedback::player_packet(bits));
                }
                HidOutput::PlayerLeds { bits, .. } => {
                    let _ = set_player_leds(&slot.pad, bits);
                }
                HidOutput::MicLed { mode, .. } if is_ds => {
                    let _ = slot.pad.send_effect(&Ds5Feedback::mic_led_packet(mode));
                }
                HidOutput::Trigger {
                    which, ref effect, ..
                } if is_ds => {
                    let _ = slot
                        .pad
                        .send_effect(&Ds5Feedback::trigger_packet(which, effect));
                }
                // Only with a live tier-A renderer: replaying volumes at a silent pad
                // would mute/blast the next session's start state.
                HidOutput::AudioCtl { flags, raw, .. } if is_ds && slot.audio_caps != 0 => {
                    let _ = slot
                        .pad
                        .send_effect(&Ds5Feedback::audio_ctl_packet(flags, &raw));
                }
                HidOutput::HidRaw { kind, data, .. } => {
                    if slot.sc2.is_none() {
                        continue;
                    }
                    if !slot.raw_rumble {
                        // The host drives the motors raw from here on; drop SDL's held level.
                        slot.raw_rumble = true;
                        let _ = slot.pad.set_rumble(0, 0, 100);
                    }
                    if let Some(cap) = &slot.sc2 {
                        cap.write(kind, data);
                    }
                }
                HidOutput::Trigger { .. }
                | HidOutput::TrackpadHaptic { .. }
                | HidOutput::AudioCtl { .. }
                | HidOutput::MicLed { .. } => {}
            }
        }
    }
}

/// Wire bitmask (low 5) → SDL player index. Every convention here spells "player N" as
/// N lit LEDs; 0-based, so player 1 is index 0. No lit LED is *no* player, not player 0.
fn player_index_from_bits(bits: u8) -> Option<u16> {
    match (bits & 0x1F).count_ones() {
        0 => None,
        n => Some((n - 1) as u16),
    }
}

fn set_player_leds(pad: &sdl3::gamepad::Gamepad, bits: u8) -> Result<(), sdl3::Error> {
    match player_index_from_bits(bits) {
        None => pad.unset_player_index(),
        Some(i) => pad.set_player_index(i),
    }
}

fn hidout_pad(h: &HidOutput) -> u8 {
    match h {
        HidOutput::Led { pad, .. }
        | HidOutput::PlayerLeds { pad, .. }
        | HidOutput::Trigger { pad, .. }
        | HidOutput::TrackpadHaptic { pad, .. }
        | HidOutput::HidRaw { pad, .. }
        | HidOutput::MicLed { pad, .. } => *pad,
        // AudioCtl's pad is the plane's only u16; decode already rejects ≥ MAX_PADS.
        HidOutput::AudioCtl { pad, .. } => *pad as u8,
    }
}

impl Worker {
    pub(super) fn new(
        subsystem: sdl3::GamepadSubsystem,
        pads_out: Arc<Mutex<Vec<PadInfo>>>,
        active_out: Arc<Mutex<Option<PadInfo>>>,
        escape_tx: async_channel::Sender<()>,
        disconnect_tx: async_channel::Sender<()>,
        menu_tx: async_channel::Sender<MenuEvent>,
        chord_tx: async_channel::Sender<(u8, SelectChord)>,
    ) -> Worker {
        Worker {
            subsystem,
            // A caller-pumped worker keeps this: its owner's loop drains rumble at its tick.
            rumble_wake: Arc::new(|| {}),
            pads_out,
            active_out,
            slots: Vec::new(),
            menu_open: Vec::new(),
            menu_last: None,
            menu_settle: MenuSettle::default(),
            battery: None,
            battery_at: None,
            order: Vec::new(),
            pinned: None,
            forwarding: true,
            gates: Gates::default(),
            kind_override: GamepadPref::Auto,
            synthetic: Vec::new(),
            pad_audio_prefs: 0,
            rumble: true,
            attached: None,
            escape_tx,
            disconnect_tx,
            chord_armed: false,
            chord_since: None,
            disconnect_fired: false,
            menu_nav: MenuNav::new(),
            menu_tx,
            chord_tx,
        }
    }
}

pub(super) fn run(
    pads_out: Arc<Mutex<Vec<PadInfo>>>,
    active_out: Arc<Mutex<Option<PadInfo>>>,
    ctl: &Receiver<Ctl>,
    escape_tx: &async_channel::Sender<()>,
    disconnect_tx: &async_channel::Sender<()>,
    menu_tx: &async_channel::Sender<MenuEvent>,
    chord_tx: &async_channel::Sender<(u8, SelectChord)>,
) -> Result<(), String> {
    // Off-main-thread, no video: keep SDL away from signals; poll pads on this thread.
    sdl3::hint::set("SDL_NO_SIGNAL_HANDLERS", "1");
    sdl3::hint::set("SDL_JOYSTICK_THREAD", "1");
    // SDL defaults the Deck HIDAPI on; mere enumeration kills lizard mode.
    set_valve_hidapi(false);
    let sdl = sdl3::init().map_err(|e| e.to_string())?;
    let subsystem = sdl.gamepad().map_err(|e| e.to_string())?;
    let mut pump = sdl.event_pump().map_err(|e| e.to_string())?;
    // A rumble update pushes an event from the demux thread, so the wait below ends at
    // once instead of at its timeout; the pass then drains the commands.
    let events = sdl.event().map_err(|e| e.to_string())?;
    events
        .register_custom_event::<RumbleWake>()
        .map_err(|e| e.to_string())?;
    let sender = events.event_sender();

    let mut w = Worker::new(
        subsystem,
        pads_out,
        active_out,
        escape_tx.clone(),
        disconnect_tx.clone(),
        menu_tx.clone(),
        chord_tx.clone(),
    );
    w.rumble_wake = Arc::new(move || {
        let _ = sender.push_custom_event(RumbleWake);
    });

    loop {
        if !w.drain_ctl(ctl) {
            return Ok(());
        }

        // Wait, don't sleep+poll. 10 ms attached/menu bounds chord-hold and haptic
        // jitter (DISCONNECT_HOLD is 1500 ms). Idle wakes at 30 ms for hotplug + ctl.
        let menu = w.gates.input_route(false).opens_menu_pads();
        let timeout = Duration::from_millis(if w.attached.is_some() || menu { 10 } else { 30 });
        if let Some(event) = pump.wait_event_timeout(timeout) {
            w.handle_event(event);
            while let Some(event) = pump.poll_event() {
                w.handle_event(event);
            }
        }

        w.gesture_poll();
        w.maybe_fire_disconnect();

        w.menu_poll();
        w.battery_poll();
        w.render_feedback();
    }
}

#[cfg(test)]
mod pref_for_ids_tests {
    use super::*;

    #[test]
    fn named_pads_declare_their_own_kind() {
        use GamepadPref as P;
        assert_eq!(pref_for_ids(0x28DE, 0x1205), Some(P::SteamDeck));
        for classic in [0x1102, 0x1142, 0x1105, 0x1106] {
            assert_eq!(pref_for_ids(0x28DE, classic), Some(P::SteamController));
        }
        assert_eq!(pref_for_ids(0x28DE, 0x1302), Some(P::SteamController2));
        assert_eq!(pref_for_ids(0x054C, 0x0DF2), Some(P::DualSenseEdge));
        for elite in [0x02E3, 0x0B00, 0x0B05, 0x0B22] {
            assert_eq!(pref_for_ids(0x045E, elite), Some(P::XboxElite));
        }
        assert_eq!(pref_for_ids(0x2DC8, 0x6012), Some(P::EightBitDoUltimate2));
        assert_eq!(pref_for_ids(0x2DC8, 0x6006), Some(P::EightBitDoPro2));
        assert_eq!(pref_for_ids(0x2DC8, 0x6009), Some(P::EightBitDoPro3));
        assert_eq!(pref_for_ids(0x0F0D, 0x0196), Some(P::HoripadSteam));
        assert_eq!(pref_for_ids(0x057E, 0x2069), Some(P::Switch2Pro));
        assert_eq!(pref_for_ids(0x057E, 0x2073), Some(P::Switch2GameCube));
        // A plain Series pad keeps SDL's type.
        assert_eq!(pref_for_ids(0x045E, 0x0B12), None);
        // X-input mode is an Xbox 360 pad.
        assert_eq!(pref_for_ids(0x2DC8, 0x310B), None);
    }
}

#[cfg(test)]
mod menu_merge_tests {
    use super::*;
    use crate::menu_nav::MENU_DEADZONE;

    fn held(dpad_right: bool, lx: i16) -> MenuSample {
        MenuSample {
            dpad: [false, false, false, dpad_right],
            lx,
            ..MenuSample::default()
        }
    }

    #[test]
    fn either_pad_drives_the_menu() {
        let idle = MenuSample::default();
        let pressing = MenuSample {
            buttons: [true, false, false, false, false, false],
            ..MenuSample::default()
        };
        // Player 2's A must survive the fold: the bug was that only one pad was ever
        // polled, so a second controller confirmed nothing until a session attached.
        assert!(merge_samples(&[idle, pressing]).buttons[0]);
        assert!(merge_samples(&[pressing, idle]).buttons[0]);
        assert!(!merge_samples(&[idle, idle]).buttons[0]);
    }

    #[test]
    fn the_furthest_stick_wins_so_idle_drift_cannot_cancel_it() {
        let pushed = held(false, 30000);
        let drifting = held(false, -300);
        assert_eq!(merge_samples(&[drifting, pushed]).lx, 30000);
        assert_eq!(merge_samples(&[pushed, drifting]).lx, 30000);
    }

    #[test]
    fn acting_needs_a_press_or_a_stick_past_the_deadzone() {
        assert!(!is_acting(&MenuSample::default()));
        assert!(!is_acting(&held(false, MENU_DEADZONE as i16 - 1)));
        assert!(is_acting(&held(false, MENU_DEADZONE as i16 + 1)));
        assert!(is_acting(&held(true, 0)));
    }
}

#[cfg(test)]
mod slot_tests {
    use super::*;

    #[test]
    fn lowest_free_index_fills_gaps_and_bounds() {
        assert_eq!(lowest_free_index(&[]), Some(0));
        assert_eq!(lowest_free_index(&[0]), Some(1));
        assert_eq!(lowest_free_index(&[0, 1, 2]), Some(3));
        // A freed middle index is reused before growing — pad 0 and pad 2 stay put.
        assert_eq!(lowest_free_index(&[0, 2]), Some(1));
        assert_eq!(lowest_free_index(&[2, 0]), Some(1));
        let all: Vec<u8> = (0..punktfunk_core::input::MAX_PADS as u8).collect();
        assert_eq!(lowest_free_index(&all), None);
        let mut but_seven = all.clone();
        but_seven.retain(|&i| i != 7);
        assert_eq!(lowest_free_index(&but_seven), Some(7));
    }

    #[test]
    fn a_deck_lists_its_built_in_controller_once() {
        let pad = |name: &str, pref, steam_virtual| PadInfo {
            name: name.into(),
            key: name.into(),
            pref,
            steam_virtual,
            battery: None,
            detail: String::new(),
            forwarded: true,
            rumble: false,
        };
        let names = |mut list: Vec<PadInfo>| {
            fold_deck_shadows(&mut list);
            list.into_iter().map(|p| p.name).collect::<Vec<_>>()
        };
        let shadow = || pad(DECK_NAME, GamepadPref::SteamDeck, true);
        let ds = || pad("DualSense", GamepadPref::DualSense, false);
        // Idle: Steam's pads only; one stands in for the built-in controls.
        assert_eq!(names(vec![shadow()]), [DECK_NAME]);
        assert_eq!(
            names(vec![shadow(), ds(), shadow()]),
            [DECK_NAME, "DualSense"]
        );
        // In session the raw 28DE:1205 is there, so every Steam pad is a shadow.
        let raw = pad(DECK_NAME, GamepadPref::SteamDeck, false);
        assert_eq!(names(vec![shadow(), raw, ds()]), [DECK_NAME, "DualSense"]);
    }

    /// Steam reads Quick Access on a pad without the button as Guide held, then A. The tap
    /// keeps that order on the wire and lets go of A first. A pad with the button taps it.
    #[test]
    fn a_quick_access_tap_holds_guide_before_it_presses_a() {
        assert_eq!(
            tap_events(wire::quick_access_presses(false)),
            [
                (wire::BTN_GUIDE, true, Duration::ZERO),
                (wire::BTN_A, true, TAP_PRESS),
                (wire::BTN_A, false, TAP_PRESS * 2),
                (wire::BTN_GUIDE, false, TAP_PRESS * 2),
            ]
        );
        assert_eq!(
            tap_events(wire::quick_access_presses(true)),
            [
                (wire::BTN_MISC1, true, Duration::ZERO),
                (wire::BTN_MISC1, false, TAP_PRESS),
            ]
        );
    }

    #[test]
    fn an_explicit_setting_is_what_every_pad_declares() {
        assert_eq!(
            declared_kind(GamepadPref::DualShock4, GamepadPref::DualSense),
            GamepadPref::DualShock4
        );
        for physical in [
            GamepadPref::DualSense,
            GamepadPref::Xbox360,
            GamepadPref::SwitchPro,
            GamepadPref::SteamDeck,
        ] {
            assert_eq!(
                declared_kind(GamepadPref::Xbox360, physical),
                GamepadPref::Xbox360
            );
        }
        assert_eq!(
            declared_kind(GamepadPref::Auto, GamepadPref::DualSense),
            GamepadPref::DualSense
        );
        assert_eq!(
            declared_kind(GamepadPref::Auto, GamepadPref::SteamDeck),
            GamepadPref::SteamDeck
        );
    }

    #[test]
    fn hidout_pad_reads_every_variant() {
        assert_eq!(
            hidout_pad(&HidOutput::Led {
                pad: 3,
                r: 1,
                g: 2,
                b: 3
            }),
            3
        );
        assert_eq!(hidout_pad(&HidOutput::PlayerLeds { pad: 5, bits: 1 }), 5);
        assert_eq!(
            hidout_pad(&HidOutput::Trigger {
                pad: 2,
                which: 0,
                effect: vec![1, 2, 3]
            }),
            2
        );
        assert_eq!(
            hidout_pad(&HidOutput::TrackpadHaptic {
                pad: 4,
                side: 0,
                amplitude: 1,
                period: 2,
                count: 3
            }),
            4
        );
        assert_eq!(
            hidout_pad(&HidOutput::HidRaw {
                pad: 6,
                kind: 0,
                data: vec![0x80, 0, 0]
            }),
            6
        );
        assert_eq!(
            hidout_pad(&HidOutput::AudioCtl {
                pad: 7,
                flags: 0,
                raw: [0; 6]
            }),
            7
        );
    }

    /// Bits 0/1 of `ucEnableBits1` stay clear: asserting either mutes the 0xD1 coils.
    #[test]
    fn speaker_enable_sets_volume_and_path_without_touching_the_haptics_bits() {
        let p = Ds5Feedback::speaker_enable_packet(0x64, 0x20);
        assert_eq!(
            p[0] & 0x03,
            0,
            "rumble-emulation / disable-audio-haptics must stay clear"
        );
        assert_eq!(
            p[0],
            0x20 | 0x80,
            "speaker-volume + audio-control validity bits"
        );
        assert_eq!(p[1], 0x80, "audio-control-2 validity bit");
        assert_eq!(p[5], 0x64);
        assert_eq!(p[7], 0x20);
        assert_eq!(p[37], 0x02, "speaker preamp +6 dB");
        for (i, b) in p.iter().enumerate() {
            if !matches!(i, 0 | 1 | 5 | 7 | 37) {
                assert_eq!(*b, 0, "byte {i} should be untouched");
            }
        }
    }

    /// A typo falls back to the default rather than silently meaning zero.
    #[test]
    fn env_u8_reads_hex_and_decimal() {
        assert_eq!(env_u8("PF_TEST_ABSENT_KEY_XYZ"), None);
        for (s, want) in [
            ("0x20", Some(0x20)),
            ("0X7f", Some(0x7F)),
            ("32", Some(32u8)),
        ] {
            let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(hex) => u8::from_str_radix(hex, 16).ok(),
                None => s.parse().ok(),
            };
            assert_eq!(parsed, want, "{s}");
        }
    }

    /// Raw bytes 5..=10 land at offsets 4..=9; bits 0/1 of `p[0]` stay clear.
    #[test]
    fn audio_ctl_folds_report_bytes_into_effect_offsets() {
        let raw = [0x50, 0x60, 0x70, 0x05, 0x11, 0x22];
        let p = Ds5Feedback::audio_ctl_packet(0b1_0111, &raw);
        assert_eq!(&p[4..10], &raw, "report bytes 5..=10 → struct 4..=9");
        assert_eq!(p[0], 0b1011_0000);
        assert_eq!(
            p[0] & 0x03,
            0,
            "haptics-select must NOT replay into p[0] bits 0/1"
        );
        assert!(p[1..4].iter().all(|&b| b == 0));
        assert!(p[10..].iter().all(|&b| b == 0));
        let p = Ds5Feedback::audio_ctl_packet(0b0_0001, &raw);
        assert_eq!(p[0], 0);
        assert_eq!(&p[4..10], &raw);
        assert_eq!(Ds5Feedback::audio_haptics_packet(), [0u8; 47]);
    }
}

#[cfg(test)]
mod menu_pad_tests {
    use super::*;

    fn pad(pref: GamepadPref, steam_virtual: bool) -> PadInfo {
        PadInfo {
            name: String::new(),
            key: String::new(),
            pref,
            steam_virtual,
            battery: None,
            detail: String::new(),
            forwarded: true,
            rumble: false,
        }
    }

    /// 1 = the raw Deck controller, 2 = Steam Input's pad for it, 3 = a DualSense.
    fn info(id: u32) -> Option<PadInfo> {
        match id {
            1 => Some(pad(GamepadPref::SteamDeck, false)),
            2 => Some(pad(GamepadPref::SteamDeck, true)),
            3 => Some(pad(GamepadPref::DualSense, false)),
            _ => None,
        }
    }

    /// Menu mode opens Steam's pad, not the raw one, while both are listed: after a session the
    /// raw pad stays in `order` until its removal arrives.
    #[test]
    fn steams_pad_stands_in_for_the_raw_deck_pad_while_both_are_listed() {
        // (ids, order) → what opens
        let rows: [(&[u32], &[u32], &[u32]); 4] = [
            (&[1, 2, 3], &[1, 2, 3], &[2, 3]),
            (&[3, 2, 1], &[3, 2, 1], &[3, 2]),
            // Steam not running: the kernel's raw pad is the Deck's only pad, and it opens.
            (&[1, 3], &[1, 3], &[1, 3]),
            (&[1], &[1, 3], &[1]),
        ];
        for (ids, order, want) in rows {
            assert_eq!(
                menu_pads(ids.to_vec(), order, info),
                want,
                "{ids:?} of {order:?}"
            );
        }
    }

    /// A pin on the raw pad opens Steam's pad while it is listed, so the launcher keeps a pad.
    #[test]
    fn a_pin_on_the_raw_deck_pad_opens_steams_pad_while_it_is_listed() {
        assert_eq!(menu_pads(vec![1], &[1, 2, 3], info), [2]);
        assert_eq!(menu_pads(vec![1], &[3, 1], info), [1]);
    }
}

#[cfg(test)]
mod player_led_tests {
    use super::*;

    /// DualSense patterns are non-contiguous; Switch/XInput is a run of low bits. Count is N.
    #[test]
    fn player_index_counts_lit_leds_for_both_conventions() {
        assert_eq!(player_index_from_bits(0x04), Some(0));
        assert_eq!(player_index_from_bits(0x0A), Some(1));
        assert_eq!(player_index_from_bits(0x15), Some(2));
        assert_eq!(player_index_from_bits(0x1B), Some(3));
        assert_eq!(player_index_from_bits(0x1F), Some(4));

        assert_eq!(player_index_from_bits(0x01), Some(0));
        assert_eq!(player_index_from_bits(0x03), Some(1));
        assert_eq!(player_index_from_bits(0x07), Some(2));
        assert_eq!(player_index_from_bits(0x0F), Some(3));
    }

    /// No lit LED is "no player", not player 0.
    #[test]
    fn no_lit_led_is_no_player() {
        assert_eq!(player_index_from_bits(0x00), None);
        assert_eq!(player_index_from_bits(0xE0), None);
    }

    #[test]
    fn high_bits_are_masked_off_before_counting() {
        assert_eq!(player_index_from_bits(0xFF), Some(4));
        assert_eq!(player_index_from_bits(0xE4), Some(0));
    }
}
