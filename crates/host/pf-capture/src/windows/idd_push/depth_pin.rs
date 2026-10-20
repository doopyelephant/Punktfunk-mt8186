//! The IDD-push display depth pin: what the session negotiated, what the display composes,
//! and when a change re-opens the driver's encoder.
//!
//! [`DepthPin::step`] takes what the shell sampled ([`Sample`]) and returns what it must do
//! ([`PinAction`]). A poller sample may ask for a pin: the shell writes it, reads the colour
//! mode back and steps [`Sample::Pinned`]. A changed descriptor asks for the topology
//! generation; two agreeing samples under one generation re-open the encoder. The settle
//! loops (≤ 250 ms) of the open and the re-arrival are the shell's own; their verdicts are
//! [`hdr_at_open`], [`hdr_reasserted`] and [`wcg_verdict`].
//!
//! Pure, so the table below runs on every target. A row per depth defect that reached a user.

use pf_win_display::snapshot::ColorMode;

/// Failed pins before the poll backs off (≈ 2 s at 4 Hz).
pub(crate) const PIN_EAGER: u32 = 8;
/// While backed off, re-pin every this-many-th sample (≈ 4 s at 4 Hz): a CCD write and query
/// take the session-global display-config lock.
pub(crate) const PIN_RETRY_EVERY: u64 = 16;

/// One poller sample, and the descriptor the encoder last opened for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplayDescriptor {
    pub(crate) hdr: bool,
    pub(crate) wcg: bool,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// What the session negotiated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Wanted {
    /// The handshake advertised HDR, not merely 10-bit. Off pins advanced colour off, so an
    /// SDR client never gets in-band PQ.
    pub(crate) hdr: bool,
    /// 10-bit SDR: wide colour where it takes, else the driver's encoder widens BGRA.
    pub(crate) ten_bit_sdr: bool,
    /// A wavelet session. Its planes are fixed, so even an HDR one pins a flip back.
    pub(crate) pyrowave: bool,
}

/// What the display composes, as the encoder last opened for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Composed {
    pub(crate) hdr: bool,
    /// SDR wide colour: FP16 under an SDR transfer.
    pub(crate) wcg: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sample {
    /// The descriptor poller's latest, its sequence, whether a topology hold is in flight, and
    /// the mode the encoder last opened at.
    Polled {
        seq: u64,
        desc: DisplayDescriptor,
        held: bool,
        mode: (u32, u32),
    },
    /// The pin this step asked for: what the write returned, and the colour mode read after.
    Pinned { set: bool, mode: Option<ColorMode> },
    /// The topology generation, read once this sample's pin is done.
    Generation(u64),
    /// A re-arrived monitor settled at this, with the negotiated depth re-asserted.
    Rearrived(Composed),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinAction {
    /// A fresh sample: re-read where DWM puts SDR white.
    RefreshSdrWhite,
    /// Write advanced colour, read the colour mode back, step [`Sample::Pinned`].
    SetAdvancedColour(bool),
    /// Write SDR wide colour, read the colour mode back, step [`Sample::Pinned`].
    SetWcg(bool),
    /// Read the topology generation, step [`Sample::Generation`].
    ReadGeneration,
    /// The display refused the negotiated depth. Once per session.
    Warn {
        observed: Option<ColorMode>,
        set: bool,
    },
    /// The descriptor changed: the next frame re-opens the driver's encoder.
    ReopenEncoder {
        from: DisplayDescriptor,
        to: DisplayDescriptor,
    },
    /// Windows refused SDR wide colour: later handshakes stop offering 10-bit SDR.
    LatchWcgRefused,
    ClearWcgRefused,
}

pub(crate) struct DepthPin {
    wanted: Wanted,
    /// Wide colour took at open, so an SDR session keeps it pinned on; off pins it off.
    wcg: bool,
    composed: Composed,
    /// The first strike: a changed descriptor and the topology generation it was seen under.
    strike: Option<(DisplayDescriptor, u64)>,
    /// The last poller sequence stepped (0 = none yet).
    seq: u64,
    /// Failed pins since the last that took or was not needed.
    failures: u32,
    warned: bool,
    /// The sample mid-step, between its pin and its generation read.
    pending: Option<(DisplayDescriptor, (u32, u32))>,
}

impl DepthPin {
    /// The pin a session starts with, at the depth its open settled at.
    pub(crate) fn new(wanted: Wanted, composed: Composed) -> Self {
        Self {
            wanted,
            wcg: wanted.ten_bit_sdr && composed.wcg,
            composed,
            strike: None,
            seq: 0,
            failures: 0,
            warned: false,
            pending: None,
        }
    }

    pub(crate) fn wanted(&self) -> Wanted {
        self.wanted
    }

    /// The wide colour an SDR session holds.
    pub(crate) fn wcg(&self) -> bool {
        self.wcg
    }

    pub(crate) fn composed(&self) -> Composed {
        self.composed
    }

    pub(crate) fn step(&mut self, sample: Sample) -> Vec<PinAction> {
        match sample {
            Sample::Polled {
                seq,
                desc,
                held,
                mode,
            } => self.polled(seq, desc, held, mode),
            Sample::Pinned { set, mode } => self.pinned(set, mode),
            Sample::Generation(generation) => self.confirm(generation),
            Sample::Rearrived(composed) => {
                self.composed = composed;
                Vec::new()
            }
        }
    }

    fn polled(
        &mut self,
        seq: u64,
        desc: DisplayDescriptor,
        held: bool,
        mode: (u32, u32),
    ) -> Vec<PinAction> {
        if seq == self.seq {
            return Vec::new();
        }
        self.seq = seq;
        let mut out = vec![PinAction::RefreshSdrWhite];
        // An exclusive-watchdog reassert is in flight: this sample is its transient eviction.
        if held {
            self.strike = None;
            return out;
        }
        self.pending = Some((desc, mode));
        match self.pin_for(desc) {
            Some(pin) if self.failures < PIN_EAGER || seq % PIN_RETRY_EVERY == 0 => {
                out.push(pin);
                return out;
            }
            Some(_) => {}
            None => self.failures = 0,
        }
        out.extend(self.compare());
        out
    }

    /// An SDR or wavelet session holds its advanced colour, and an SDR session its wide
    /// colour, in either direction. HDR H.26x follows a flip: its encoder re-opens.
    fn pin_for(&self, d: DisplayDescriptor) -> Option<PinAction> {
        let want = self.wanted.hdr;
        if (self.wanted.pyrowave || !want) && d.hdr != want {
            return Some(PinAction::SetAdvancedColour(want));
        }
        if !want && !d.hdr && d.wcg != self.wcg {
            return Some(PinAction::SetWcg(self.wcg));
        }
        None
    }

    /// The pin is observed, never assumed: a display that cannot flip keeps what it composes,
    /// and the encoder follows that.
    fn pinned(&mut self, set: bool, mode: Option<ColorMode>) -> Vec<PinAction> {
        let Some((mut now, at)) = self.pending else {
            return Vec::new();
        };
        // A failed read is no evidence of a failed pin: the poller's sample stands.
        if let Some(mode) = mode {
            now.hdr = mode == ColorMode::Hdr;
            now.wcg = mode == ColorMode::Wcg;
        }
        self.pending = Some((now, at));
        let want = self.wanted.hdr;
        let mut out = Vec::new();
        if now.hdr != want || (!want && now.wcg != self.wcg) {
            self.failures = self.failures.saturating_add(1);
            if !self.warned {
                self.warned = true;
                out.push(PinAction::Warn {
                    observed: mode,
                    set,
                });
            }
        } else {
            self.failures = 0;
        }
        out.extend(self.compare());
        out
    }

    fn compare(&mut self) -> Vec<PinAction> {
        let Some((now, mode)) = self.pending else {
            return Vec::new();
        };
        if now == self.descriptor(mode) {
            self.strike = None;
            self.pending = None;
            return Vec::new();
        }
        vec![PinAction::ReadGeneration]
    }

    /// Two strikes straddling a finished topology transaction are two desktops, never one
    /// confirmed change.
    fn confirm(&mut self, generation: u64) -> Vec<PinAction> {
        let Some((now, mode)) = self.pending.take() else {
            return Vec::new();
        };
        if self.strike != Some((now, generation)) {
            self.strike = Some((now, generation));
            return Vec::new();
        }
        self.strike = None;
        let from = self.descriptor(mode);
        self.composed = Composed {
            hdr: now.hdr,
            wcg: now.wcg,
        };
        vec![PinAction::ReopenEncoder { from, to: now }]
    }

    fn descriptor(&self, (width, height): (u32, u32)) -> DisplayDescriptor {
        DisplayDescriptor {
            hdr: self.composed.hdr,
            wcg: self.composed.wcg,
            width,
            height,
        }
    }
}

/// The depth an open settles at. A 10-bit session takes the successful write: a 250 ms read
/// can still say SDR while the driver composes FP16. A failed read is SDR.
pub(crate) fn hdr_at_open(want: bool, set: bool, observed: Option<bool>) -> bool {
    want && (set || observed.unwrap_or(false))
}

/// The depth a re-arrived monitor settles at: what it reads, else what the write answered.
pub(crate) fn hdr_reasserted(want: bool, set: bool, observed: Option<bool>) -> bool {
    observed.unwrap_or(want && set)
}

/// What a wide-colour write that settled at `got` means for the refusal latch. `mode` is the
/// colour mode read after a miss; `None`, a path not yet active after a re-arrival, is no
/// refusal.
pub(crate) fn wcg_verdict(want: bool, got: bool, mode: Option<ColorMode>) -> Option<PinAction> {
    match (want, got) {
        (true, true) => Some(PinAction::ClearWcgRefused),
        (true, false) if mode.is_some() => Some(PinAction::LatchWcgRefused),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ColorMode::{Hdr, Sdr, Wcg};
    use PinAction::{
        ClearWcgRefused, LatchWcgRefused, ReadGeneration, RefreshSdrWhite, ReopenEncoder,
        SetAdvancedColour, SetWcg, Warn,
    };

    const MODE: (u32, u32) = (1920, 1080);
    const SDR: Wanted = Wanted {
        hdr: false,
        ten_bit_sdr: false,
        pyrowave: false,
    };
    const SDR10: Wanted = Wanted {
        ten_bit_sdr: true,
        ..SDR
    };
    const HDR: Wanted = Wanted { hdr: true, ..SDR };
    const WAVELET_HDR: Wanted = Wanted {
        pyrowave: true,
        ..HDR
    };

    fn composed(hdr: bool, wcg: bool) -> Composed {
        Composed { hdr, wcg }
    }

    fn desc(hdr: bool, wcg: bool, (width, height): (u32, u32)) -> DisplayDescriptor {
        DisplayDescriptor {
            hdr,
            wcg,
            width,
            height,
        }
    }

    fn polled(seq: u64, hdr: bool, wcg: bool) -> Sample {
        polled_at(seq, desc(hdr, wcg, MODE))
    }

    fn polled_at(seq: u64, desc: DisplayDescriptor) -> Sample {
        Sample::Polled {
            seq,
            desc,
            held: false,
            mode: MODE,
        }
    }

    fn held(seq: u64, hdr: bool) -> Sample {
        Sample::Polled {
            seq,
            desc: desc(hdr, false, MODE),
            held: true,
            mode: MODE,
        }
    }

    fn pinned(set: bool, mode: Option<ColorMode>) -> Sample {
        Sample::Pinned { set, mode }
    }

    fn reopen(from: DisplayDescriptor, to: DisplayDescriptor) -> PinAction {
        ReopenEncoder { from, to }
    }

    struct Row {
        /// The behaviour this row pins.
        pins: &'static str,
        wanted: Wanted,
        open: Composed,
        /// Each sample with the actions it must produce.
        script: Vec<(Sample, Vec<PinAction>)>,
        /// What the encoder opens for at the end.
        end: Composed,
    }

    fn table() -> Vec<Row> {
        let small = (1280, 720);
        vec![
            Row {
                pins: "an SDR session holds advanced colour off: a flip to HDR is pinned back",
                wanted: SDR,
                open: composed(false, false),
                script: vec![
                    (
                        polled(1, true, false),
                        vec![RefreshSdrWhite, SetAdvancedColour(false)],
                    ),
                    (pinned(true, Some(Sdr)), vec![]),
                ],
                end: composed(false, false),
            },
            Row {
                pins: "a 10-bit SDR session that took wide colour holds it on",
                wanted: SDR10,
                open: composed(false, true),
                script: vec![
                    (polled(1, false, false), vec![RefreshSdrWhite, SetWcg(true)]),
                    (pinned(true, Some(Wcg)), vec![]),
                ],
                end: composed(false, true),
            },
            Row {
                pins: "a 10-bit SDR session refused wide colour widens, and holds wide colour off",
                wanted: SDR10,
                open: composed(false, false),
                script: vec![
                    (polled(1, false, true), vec![RefreshSdrWhite, SetWcg(false)]),
                    (pinned(true, Some(Sdr)), vec![]),
                ],
                end: composed(false, false),
            },
            Row {
                pins: "an 8-bit SDR session holds wide colour off",
                wanted: SDR,
                open: composed(false, false),
                script: vec![
                    (polled(1, false, true), vec![RefreshSdrWhite, SetWcg(false)]),
                    (pinned(true, Some(Sdr)), vec![]),
                ],
                end: composed(false, false),
            },
            Row {
                pins: "337faf10f: an HDR flip at one geometry re-opens on two samples, not one",
                wanted: HDR,
                open: composed(true, false),
                script: vec![
                    (
                        polled(1, false, false),
                        vec![RefreshSdrWhite, ReadGeneration],
                    ),
                    (Sample::Generation(0), vec![]),
                    (
                        polled(2, false, false),
                        vec![RefreshSdrWhite, ReadGeneration],
                    ),
                    (
                        Sample::Generation(0),
                        vec![reopen(desc(true, false, MODE), desc(false, false, MODE))],
                    ),
                    (polled(3, false, false), vec![RefreshSdrWhite]),
                ],
                end: composed(false, false),
            },
            Row {
                pins: "a mode a game sets re-opens the encoder at the new geometry",
                wanted: HDR,
                open: composed(true, false),
                script: vec![
                    (
                        polled_at(1, desc(true, false, small)),
                        vec![RefreshSdrWhite, ReadGeneration],
                    ),
                    (Sample::Generation(4), vec![]),
                    (
                        polled_at(2, desc(true, false, small)),
                        vec![RefreshSdrWhite, ReadGeneration],
                    ),
                    (
                        Sample::Generation(4),
                        vec![reopen(desc(true, false, MODE), desc(true, false, small))],
                    ),
                ],
                end: composed(true, false),
            },
            Row {
                pins:
                    "a topology re-probe blip: two samples under two generations are two desktops",
                wanted: HDR,
                open: composed(true, false),
                script: vec![
                    (
                        polled(1, false, false),
                        vec![RefreshSdrWhite, ReadGeneration],
                    ),
                    (Sample::Generation(0), vec![]),
                    (
                        polled(2, false, false),
                        vec![RefreshSdrWhite, ReadGeneration],
                    ),
                    (Sample::Generation(1), vec![]),
                    (
                        polled(3, false, false),
                        vec![RefreshSdrWhite, ReadGeneration],
                    ),
                    (
                        Sample::Generation(1),
                        vec![reopen(desc(true, false, MODE), desc(false, false, MODE))],
                    ),
                ],
                end: composed(false, false),
            },
            Row {
                pins: "a topology hold drops the first strike, and its sample is no strike",
                wanted: HDR,
                open: composed(true, false),
                script: vec![
                    (
                        polled(1, false, false),
                        vec![RefreshSdrWhite, ReadGeneration],
                    ),
                    (Sample::Generation(0), vec![]),
                    (held(2, false), vec![RefreshSdrWhite]),
                    (
                        polled(3, false, false),
                        vec![RefreshSdrWhite, ReadGeneration],
                    ),
                    (Sample::Generation(0), vec![]),
                ],
                end: composed(true, false),
            },
            Row {
                pins: "a sample back at the open descriptor drops the strike",
                wanted: HDR,
                open: composed(true, false),
                script: vec![
                    (
                        polled(1, false, false),
                        vec![RefreshSdrWhite, ReadGeneration],
                    ),
                    (Sample::Generation(0), vec![]),
                    (polled(2, true, false), vec![RefreshSdrWhite]),
                    (
                        polled(3, false, false),
                        vec![RefreshSdrWhite, ReadGeneration],
                    ),
                    (Sample::Generation(0), vec![]),
                ],
                end: composed(true, false),
            },
            Row {
                pins: "a poller sequence seen before is no sample",
                wanted: SDR,
                open: composed(false, false),
                script: vec![
                    (polled(1, false, false), vec![RefreshSdrWhite]),
                    (polled(1, true, false), vec![]),
                ],
                end: composed(false, false),
            },
            Row {
                pins:
                    "5a0a7adc1: a refused pin warns once, then the encoder follows what is composed",
                wanted: SDR,
                open: composed(false, false),
                script: vec![
                    (
                        polled(1, true, false),
                        vec![RefreshSdrWhite, SetAdvancedColour(false)],
                    ),
                    (
                        pinned(false, Some(Hdr)),
                        vec![
                            Warn {
                                observed: Some(Hdr),
                                set: false,
                            },
                            ReadGeneration,
                        ],
                    ),
                    (Sample::Generation(0), vec![]),
                    (
                        polled(2, true, false),
                        vec![RefreshSdrWhite, SetAdvancedColour(false)],
                    ),
                    (pinned(true, Some(Hdr)), vec![ReadGeneration]),
                    (
                        Sample::Generation(0),
                        vec![reopen(desc(false, false, MODE), desc(true, false, MODE))],
                    ),
                    (
                        polled(3, true, false),
                        vec![RefreshSdrWhite, SetAdvancedColour(false)],
                    ),
                    (pinned(true, Some(Hdr)), vec![]),
                ],
                end: composed(true, false),
            },
            Row {
                pins: "a failed read after a pin keeps the poller's sample",
                wanted: SDR,
                open: composed(false, false),
                script: vec![
                    (
                        polled(1, true, false),
                        vec![RefreshSdrWhite, SetAdvancedColour(false)],
                    ),
                    (
                        pinned(true, None),
                        vec![
                            Warn {
                                observed: None,
                                set: true,
                            },
                            ReadGeneration,
                        ],
                    ),
                    (Sample::Generation(0), vec![]),
                ],
                end: composed(false, false),
            },
            Row {
                pins: "a wavelet HDR session pins a flip to SDR back",
                wanted: WAVELET_HDR,
                open: composed(true, false),
                script: vec![
                    (
                        polled(1, false, false),
                        vec![RefreshSdrWhite, SetAdvancedColour(true)],
                    ),
                    (pinned(true, Some(Hdr)), vec![]),
                ],
                end: composed(true, false),
            },
            Row {
                pins: "the re-arrival re-assert is adopted at once, with no debounce",
                wanted: HDR,
                open: composed(true, false),
                script: vec![
                    (Sample::Rearrived(composed(false, false)), vec![]),
                    (polled(1, false, false), vec![RefreshSdrWhite]),
                ],
                end: composed(false, false),
            },
        ]
    }

    #[test]
    fn each_depth_fix_holds() {
        for row in table() {
            let mut pin = DepthPin::new(row.wanted, row.open);
            for (i, (sample, want)) in row.script.iter().enumerate() {
                assert_eq!(
                    &pin.step(*sample),
                    want,
                    "{}: step {i} {sample:?}",
                    row.pins
                );
            }
            assert_eq!(pin.composed(), row.end, "{}: end", row.pins);
        }
    }

    /// A display stuck in HDR under an SDR session: eight eager pins, then one every
    /// [`PIN_RETRY_EVERY`]th sample, one warning; a pin that takes starts eager again.
    #[test]
    fn a_refused_pin_retries_eagerly_then_every_nth() {
        let mut pin = DepthPin::new(SDR, composed(false, false));
        let mut pinned_at = Vec::new();
        let mut warns = 0;
        for seq in 1..=40 {
            let mut queue = std::collections::VecDeque::from(pin.step(polled(seq, true, false)));
            while let Some(action) = queue.pop_front() {
                let next = match action {
                    SetAdvancedColour(false) => {
                        pinned_at.push(seq);
                        pinned(false, Some(Hdr))
                    }
                    ReadGeneration => Sample::Generation(0),
                    Warn { .. } => {
                        warns += 1;
                        continue;
                    }
                    RefreshSdrWhite | ReopenEncoder { .. } => continue,
                    other => panic!("{other:?}"),
                };
                queue.extend(pin.step(next));
            }
        }
        assert_eq!(pinned_at, [1, 2, 3, 4, 5, 6, 7, 8, 16, 32]);
        assert_eq!(warns, 1);

        // The display takes the pin: the count resets, and a later refusal is eager again.
        assert_eq!(
            pin.step(polled(48, true, false)),
            [RefreshSdrWhite, SetAdvancedColour(false)]
        );
        pin.step(pinned(true, Some(Sdr)));
        for seq in 49..=56 {
            let actions = pin.step(polled(seq, true, false));
            assert!(actions.contains(&SetAdvancedColour(false)), "seq {seq}");
            pin.step(pinned(false, Some(Hdr)));
        }
    }

    #[test]
    fn the_open_and_re_arrival_verdicts_hold() {
        let rows: &[(&str, bool, bool, Option<bool>, bool)] = &[
            (
                "a 10-bit open takes the write: the read can lag",
                true,
                true,
                Some(false),
                true,
            ),
            (
                "a 10-bit open whose write failed reads HDR",
                true,
                false,
                Some(true),
                true,
            ),
            (
                "a 10-bit open with a failed read and write is SDR",
                true,
                false,
                None,
                false,
            ),
            (
                "an SDR open is SDR whatever the display reads",
                false,
                false,
                Some(true),
                false,
            ),
        ];
        for &(pins, want, set, observed, got) in rows {
            assert_eq!(hdr_at_open(want, set, observed), got, "{pins}");
        }
        let rows: &[(&str, bool, bool, Option<bool>, bool)] = &[
            (
                "a re-arrival takes the read",
                true,
                true,
                Some(false),
                false,
            ),
            (
                "a failed read takes the write's answer",
                true,
                true,
                None,
                true,
            ),
            ("a failed read and write is SDR", true, false, None, false),
            (
                "an SDR re-arrival with a failed read is SDR",
                false,
                true,
                None,
                false,
            ),
            (
                "an SDR re-arrival still reading HDR is HDR",
                false,
                true,
                Some(true),
                true,
            ),
        ];
        for &(pins, want, set, observed, got) in rows {
            assert_eq!(hdr_reasserted(want, set, observed), got, "{pins}");
        }
        type WcgRow = (
            &'static str,
            bool,
            bool,
            Option<ColorMode>,
            Option<PinAction>,
        );
        let rows: &[WcgRow] = &[
            (
                "wide colour that takes clears the latch",
                true,
                true,
                None,
                Some(ClearWcgRefused),
            ),
            (
                "wide colour refused on a readable mode latches",
                true,
                false,
                Some(Sdr),
                Some(LatchWcgRefused),
            ),
            (
                "968f66e5c: a mode read as None after a re-arrival is no refusal",
                true,
                false,
                None,
                None,
            ),
            (
                "turning wide colour off touches no latch",
                false,
                false,
                None,
                None,
            ),
            (
                "wide colour stuck on touches no latch",
                false,
                true,
                Some(Wcg),
                None,
            ),
        ];
        for &(pins, want, got, mode, verdict) in rows {
            assert_eq!(wcg_verdict(want, got, mode), verdict, "{pins}");
        }
    }
}
