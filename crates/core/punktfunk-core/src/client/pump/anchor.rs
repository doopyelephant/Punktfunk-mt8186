//! The client's mode, moved by the host's `StreamConfig` at the first frame of the epoch it names,
//! and each epoch's video format. A `Reconfigured` ack says a switch was accepted; the config
//! says what each epoch delivers.

use super::{PumpEvent, PumpTx};
use crate::config::Mode;
use crate::quic::v2::msg::StreamConfig;
use crate::quic::VideoFormat;
use std::sync::Mutex;

/// Epochs whose format is kept. The host moves one epoch per pipeline swap, so a frame never
/// trails its config by this many.
const BOOKED: usize = 8;

#[derive(Debug, Default)]
pub(crate) struct EpochAnchor {
    /// The host sends a config per epoch (`FEATURE_STREAM_CONFIG` in force).
    pub(crate) on: bool,
    /// An epoch may change the format (`FEATURE_FORMAT_EPOCH` in force): frames wait for
    /// their epoch's config before they go to the decoder.
    pub(crate) formats: bool,
    /// A config whose epoch has shown no frame yet.
    pending: Option<StreamConfig>,
    /// The newest epoch a frame carried.
    seen: Option<u8>,
    /// The format of each recent epoch, newest last.
    booked: Vec<(u8, VideoFormat)>,
}

impl EpochAnchor {
    /// A config arrived. `Some(mode)` when its epoch is already on screen.
    pub(crate) fn config(&mut self, cfg: StreamConfig) -> Option<Mode> {
        self.book(cfg.epoch, cfg.format());
        if self.seen == Some(cfg.epoch) {
            self.pending = None;
            return Some(cfg.mode);
        }
        self.pending = Some(cfg);
        None
    }

    /// The first frame of `epoch` arrived. `Some(mode)` when its config came first.
    pub(crate) fn frame(&mut self, epoch: u8) -> Option<Mode> {
        self.seen = Some(epoch);
        Some(self.pending.take_if(|c| c.epoch == epoch)?.mode)
    }

    /// `epoch` decodes as `format` from here on. A later booking of the same epoch replaces it.
    pub(crate) fn book(&mut self, epoch: u8, format: VideoFormat) {
        self.booked.retain(|(e, _)| *e != epoch);
        if self.booked.len() == BOOKED {
            self.booked.remove(0);
        }
        self.booked.push((epoch, format));
    }

    /// `epoch`'s config never came: it decodes as the newest epoch that has one.
    pub(crate) fn assume(&mut self, epoch: u8) {
        if let Some(&(_, f)) = self.booked.last() {
            self.book(epoch, f);
        }
    }

    /// `epoch`'s format, `None` before its config arrived.
    pub(crate) fn format(&self, epoch: u8) -> Option<VideoFormat> {
        self.booked
            .iter()
            .rev()
            .find_map(|(e, f)| (*e == epoch).then_some(*f))
    }

    /// A frame of `epoch` may go to the decoder: no format moves, or its epoch's is known.
    pub(crate) fn ready(&self, epoch: u8) -> bool {
        !self.formats || self.format(epoch).is_some()
    }
}

/// Under format epochs, mark the first frame handed out whose epoch decodes as another format
/// than the frame before it, and make that format the live one.
pub(crate) fn mark(shared: &super::super::ClientShared, f: &mut crate::session::Frame) {
    let Some(format) = ({
        let anchor = shared.anchor.lock().unwrap_or_else(|e| e.into_inner());
        anchor.formats.then(|| anchor.format(f.epoch)).flatten()
    }) else {
        return;
    };
    let mut live = shared
        .video_format
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if *live != format {
        *live = format;
        f.new_format = Some(format);
        tracing::info!(
            epoch = f.epoch,
            codec = crate::hud::codec_label(format.codec),
            bit_depth = format.bit_depth,
            transfer = format.color.transfer,
            chroma = format.chroma_format,
            "the host changed the video format — rebuilding the decoder"
        );
    }
}

/// Point the mode slot at `mode`. A change tells the pump, which resets mode-scoped ABR state.
pub(super) fn apply(slot: &Mutex<Mode>, events: &PumpTx, mode: Mode) {
    let mut m = slot.lock().unwrap_or_else(|e| e.into_inner());
    if *m != mode {
        *m = mode;
        let _ = events.send(PumpEvent::Mode(mode));
        tracing::info!(?mode, "host delivered mode switch");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::{CODEC_H264, CODEC_HEVC};

    fn cfg(epoch: u8, width: u32) -> StreamConfig {
        StreamConfig {
            epoch,
            mode: Mode {
                width,
                height: 720,
                refresh_hz: 60,
            },
            ..StreamConfig::default()
        }
    }

    /// The config and the epoch's first frame race; the mode moves when the second of them lands.
    #[test]
    fn the_mode_moves_when_config_and_frame_have_both_arrived() {
        let mut a = EpochAnchor::default();
        assert_eq!(a.frame(0), None);
        assert_eq!(a.config(cfg(1, 1280)), None, "no frame of epoch 1 yet");
        assert_eq!(a.frame(1).map(|m| m.width), Some(1280));
        assert_eq!(a.frame(1), None, "applied once");

        assert_eq!(a.frame(2), None, "a frame ahead of its config");
        assert_eq!(a.config(cfg(2, 1920)).map(|m| m.width), Some(1920));
    }

    /// A config for an epoch whose frames never came is replaced by the next one.
    #[test]
    fn a_skipped_epoch_never_moves_the_mode() {
        let mut a = EpochAnchor::default();
        assert_eq!(a.config(cfg(3, 800)), None);
        assert_eq!(a.config(cfg(4, 1024)), None);
        assert_eq!(a.frame(4).map(|m| m.width), Some(1024));
    }

    /// Under format epochs a frame waits until its epoch's config is in; without them it never
    /// waits.
    #[test]
    fn a_frame_waits_for_its_epochs_format_only_under_format_epochs() {
        let mut a = EpochAnchor::default();
        assert!(a.ready(5), "nothing moves the format");
        a.formats = true;
        assert!(!a.ready(5));
        let mut c = cfg(5, 1280);
        c.codec = CODEC_H264;
        a.config(c);
        assert!(a.ready(5));
        assert_eq!(a.format(5).map(|f| f.codec), Some(CODEC_H264));
        assert!(!a.ready(6), "the next epoch waits for its own");
    }

    /// Old epochs fall out; a re-sent config replaces its epoch's booking.
    #[test]
    fn bookings_keep_the_newest_epochs() {
        let mut a = EpochAnchor::default();
        let f = |codec| VideoFormat {
            codec,
            ..VideoFormat::default()
        };
        for e in 0..=BOOKED as u8 {
            a.book(e, f(CODEC_HEVC));
        }
        assert_eq!(a.format(0), None, "evicted");
        a.book(3, f(CODEC_H264));
        assert_eq!(a.format(3).map(|f| f.codec), Some(CODEC_H264));
        assert_eq!(a.booked.len(), BOOKED);
    }

    /// The first frame handed out in a new format is marked, once, and the format goes live;
    /// without format epochs nothing is marked.
    #[test]
    fn the_first_frame_in_a_new_format_is_marked_once() {
        let shared = super::super::super::ClientShared::new(Mode::default());
        let hevc = VideoFormat {
            codec: CODEC_HEVC,
            ..VideoFormat::default()
        };
        *shared.video_format.lock().unwrap() = hevc;
        {
            let mut a = shared.anchor.lock().unwrap();
            a.book(0, hevc);
            let mut c = cfg(1, 1280);
            c.codec = CODEC_H264;
            a.config(c);
        }
        let marked = |epoch| {
            let mut f = crate::session::Frame {
                data: Vec::new().into(),
                frame_index: 0,
                pts_ns: 0,
                flags: 0,
                epoch,
                complete: true,
                part: None,
                received_ns: 0,
                new_format: None,
            };
            mark(&shared, &mut f);
            f.new_format.map(|f| f.codec)
        };
        assert_eq!(marked(1), None, "no format epochs");
        shared.anchor.lock().unwrap().formats = true;
        assert_eq!(marked(0), None);
        assert_eq!(marked(1), Some(CODEC_H264));
        assert_eq!(marked(1), None, "marked once");
        assert_eq!(shared.video_format.lock().unwrap().codec, CODEC_H264);
    }

    #[test]
    fn applying_the_same_mode_tells_the_pump_nothing() {
        let slot = Mutex::new(cfg(0, 1280).mode);
        let (tx, rx) = std::sync::mpsc::channel();
        apply(&slot, &tx, cfg(0, 1280).mode);
        assert_eq!(rx.try_iter().count(), 0);
        apply(&slot, &tx, cfg(0, 1920).mode);
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            [PumpEvent::Mode(cfg(0, 1920).mode)]
        );
        assert_eq!(slot.lock().unwrap().width, 1920);
    }
}
