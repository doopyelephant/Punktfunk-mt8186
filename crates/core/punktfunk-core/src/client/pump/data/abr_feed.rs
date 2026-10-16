//! The data pump's ABR side: what [`crate::abr::Driver`] hears each iteration, what its tick
//! asks for, and the windows it closes, published for the embedder.

use super::*;
use crate::abr::{Action, DriverConfig, ProbeReport};

/// Closed windows held for an embedder that has not read them. Forty-eight
/// seconds at the report cadence: enough that a client polling once a second
/// never loses one, small enough that one which never polls costs nothing.
pub(crate) const ABR_TRAJECTORY_WINDOWS: usize = 64;

impl DataPump {
    /// The session's ABR driver, with the three environment overrides read
    /// once. Automatic is a session with no embedder rate and a host that
    /// echoed one: PyroWave's adapts between its floor and the host's pin.
    /// An explicit-rate PyroWave session runs at the pin, which the bring-up
    /// ramp may lower once; no controller runs for it.
    pub(super) fn driver(&self, session_start: Instant) -> crate::abr::Driver {
        let automatic = self.bitrate_kbps == 0;
        let pyrowave = self.negotiated_codec == crate::quic::CODEC_PYROWAVE;
        let pin_kbps = (pyrowave && !automatic)
            .then_some(self.resolved_bitrate_kbps)
            .filter(|&pin| pin > 0);
        let mut driver = crate::abr::Driver::new(
            DriverConfig {
                start_kbps: if automatic {
                    self.resolved_bitrate_kbps
                } else {
                    0
                },
                ceiling_cap_kbps: env_u32("PUNKTFUNK_ABR_MAX_MBPS")
                    .map(|m| m.saturating_mul(1_000)),
                stream_cap_kbps: self.stream_cap_kbps,
                refresh_hz: self.refresh_hz,
                codec: self.negotiated_codec,
                bit_depth: self.bit_depth,
                chroma_format: self.chroma_format,
                audio_reserved_kbps: self.audio_reserved_kbps,
                marks_repeats: self.marks_repeats,
                probe: std::env::var("PUNKTFUNK_ABR_PROBE").map_or(true, |v| v != "0"),
                probe_target_kbps: env_u32("PUNKTFUNK_ABR_PROBE_KBPS"),
                ramp: self.serves_ramp,
                probe_only: self.shared.probe_only(),
                pin_kbps,
                floor_kbps: self.pyrowave_floor_kbps.filter(|_| automatic),
            },
            session_start,
        );
        driver.set_ports(
            self.shared.host_link.lock().unwrap().facts(),
            *self.shared.client_link.lock().unwrap(),
        );
        driver
    }

    /// Everything the driver hears before this iteration's tick, in the order it hears it.
    /// The control task's events are drained once, at the top. Returns whether a probe burst
    /// is in flight.
    pub(super) fn feed_driver(&mut self, lp: &mut PumpLoop, clock_offset_ns: i64) -> bool {
        let events = drain_events(&self.events);
        // Re-sync invalidates the staleness run under the old offset.
        if events.resynced {
            // Every OWD reading shifted with the offset; the old floor is
            // meaningless. A stale offset that WAS the elevation is fixed here.
            lp.standing_lat.rebase();
            if lp.jump.rebase() {
                tracing::info!("clock re-sync applied — clock-based jump-to-live re-armed");
            }
        }
        // Drain here, not at the report tick, so the in-flight window
        // (the one the rebuild corrupted) is the one we can still drop.
        if let Some(gap_ms) = events.gap_ms {
            lp.abr.on_pipeline_gap(gap_ms);
        }
        // Mirror drop/FEC counters every iteration, not only on a
        // produced frame — a total-loss drought completes no AU.
        let st = self.session.stats();
        lp.abr.on_stats(&st);
        lp.abr.on_loss_positions(self.session.take_loss_positions());
        let host = *self.shared.host_link.lock().unwrap();
        if host != lp.host_link {
            lp.host_link = host;
            lp.abr
                .set_ports(host.facts(), *self.shared.client_link.lock().unwrap());
        }
        // One syscall per sample: often enough for a window, rare enough for the hot loop. The
        // receiver's drops are the kernel socket's, where the OS keeps a figure, and the demux
        // queue's.
        if lp.sock_read.elapsed() >= Duration::from_millis(100) {
            lp.sock_read = Instant::now();
            let kernel = self
                .shared
                .data_sock
                .lock()
                .unwrap()
                .as_ref()
                .and_then(crate::transport::sockstat::socket_drops);
            let demux = self
                .shared
                .demux
                .lock()
                .unwrap()
                .as_ref()
                .map_or(0, |s| s.media_dropped.load(Ordering::Relaxed));
            lp.abr.on_sock_drops(kernel.unwrap_or(0) + demux);
        }
        // One delay sample per frame that opened since the last iteration,
        // whether or not it ever completed. Same offset and same sign test
        // as a completed AU's; without an offset there is no delay to read,
        // but the samples are still drained.
        for raw_ns in self.session.take_shard_delays() {
            let owd_ns = i128::from(raw_ns) + i128::from(clock_offset_ns);
            if clock_offset_ns != 0 && owd_ns > 0 {
                lp.abr.on_shard_owd(owd_ns);
            }
        }
        lp.watch_ingress(&st);
        self.shared
            .frames_dropped
            .store(st.frames_dropped, Ordering::Relaxed);
        self.shared
            .fec_recovered
            .store(st.fec_recovered_shards, Ordering::Relaxed);
        let (probe_active, probe_duration_ms, probe_report) = self.mirror_probe(&st);
        lp.abr
            .on_probe_active(probe_active, probe_duration_ms, Instant::now());
        if let Some(r) = probe_report {
            lp.abr.on_probe_result(r, Instant::now());
        }
        if let Some(m) = events.mode {
            lp.abr.on_mode_switch(m.width, m.height, m.refresh_hz);
            self.shared
                .feedback
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_refresh(m.refresh_hz);
        }
        for (acked, why) in events.acks {
            lp.abr.on_ack(acked, why);
        }
        // Drain even when the controller is off, so the accumulators
        // stay bounded and no count leaks into a later window.
        let dec = std::mem::take(&mut *self.shared.decode_lat.lock().unwrap());
        lp.abr.on_decode_latency(dec.sum_us, dec.count);
        let enc = std::mem::take(&mut *self.encode_lat.lock().unwrap());
        lp.abr.on_encode_latency(enc.sum_us, enc.count);
        lp.abr
            .on_keyframe_asks(self.shared.recovery_kf.swap(0, Ordering::Relaxed));
        probe_active
    }

    /// Copy the session's probe counters into the shared [`ProbeState`].
    /// Returns `(active, duration_ms, report)`; the report stands while the
    /// state says done.
    fn mirror_probe(&mut self, st: &crate::stats::Stats) -> (bool, u32, Option<ProbeReport>) {
        let mut p = self.shared.probe.lock().unwrap();
        if p.active && !p.done {
            // An embedder speed test is armed on its first mirror
            // tick, which can miss packets a fast link returned in
            // the meantime. The pump's own probes arrive armed.
            let arming = p.base_bytes.is_none();
            if arming {
                self.session.reset_probe_arrivals();
            }
            p.rx_packets_now = st.probe_packets_received;
            p.rx_bytes_now = st.probe_bytes_received;
            (p.first_arrival_ns, p.last_arrival_ns) = if arming {
                (0, 0)
            } else {
                (st.probe_first_arrival_ns, st.probe_last_arrival_ns)
            };
            // The snapshot predates the arm's reset; its gaps belong to the burst before.
            if !arming {
                p.gap_buckets = st.probe_gap_buckets;
                p.reorders = st.probe_reorders;
            }
            p.base_packets.get_or_insert(st.probe_packets_received);
            p.base_bytes.get_or_insert(st.probe_bytes_received);
        } else if p.done && p.ramp {
            // The host's report rides the control stream and the
            // filler rides the data plane, so it can arrive while the
            // bottleneck queue is still handing us the step. Keep
            // counting: the ramp reads the drain, not the send window.
            p.refresh_delivered(st);
        }
        let report = p.done.then(|| ProbeReport {
            delivered_bytes: p.delivered_bytes,
            delivered_packets: p.delivered_packets,
            window_ms: p.throughput_window_ms(p.delivered_packets),
            host_duration_ms: p.host_duration_ms,
            client_interval_ms: p.client_interval_ms,
            client_interval_us: p.client_interval_us,
            host_bytes_sent: p.host_goodput_bytes,
            wire_packets_sent: p.host_wire_packets,
            send_dropped: p.host_send_dropped,
        });
        (p.active && !p.done, p.duration_ms, report)
    }

    /// Send what the tick asked for. Returns the rate this window asked
    /// for, recorded beside the window it came out of.
    pub(super) fn dispatch(
        &mut self,
        abr: &mut crate::abr::Driver,
        actions: Vec<Action>,
    ) -> Option<u32> {
        let mut request_kbps = None;
        for action in actions {
            match action {
                Action::Report {
                    loss_ppm,
                    packets_received,
                    head,
                    mid,
                    tail,
                    sock_drops,
                } => {
                    let report = crate::quic::v2::dgram::Feedback {
                        loss_ppm,
                        packets_received,
                        head,
                        mid,
                        tail,
                        sock_drops,
                        ..Default::default()
                    };
                    let fb = self.shared.feedback.lock().unwrap().window(report);
                    self.shared.send_feedback(&fb);
                }
                Action::Shape(shape) => {
                    let wake = shape == crate::abr::Shape::Wake as u8;
                    self.shared.wake_shape.store(wake, Ordering::Relaxed);
                    self.shared.feedback.lock().unwrap().shape(shape);
                }
                Action::LinkRate(kbps) => {
                    *self.shared.link.lock().unwrap() = abr.link();
                    let fb = self.shared.feedback.lock().unwrap().link(kbps);
                    if let Some(fb) = fb {
                        self.shared.send_feedback(&fb);
                    }
                }
                Action::SetBitrate(kbps) => {
                    request_kbps = Some(kbps);
                    if self
                        .ctrl_tx
                        .try_send(CtrlRequest::SetBitrate(kbps))
                        .is_err()
                    {
                        // Never reached the control task. Three of these
                        // retire the controller as "the host never acked".
                        abr.on_request_dropped(kbps);
                    }
                }
                Action::Keyframe => self.shared.ask_keyframe(),
                Action::Probe {
                    target_kbps,
                    duration_ms,
                    ramp,
                } => {
                    // One ProbeState and no correlation id: an embedder
                    // speed test in flight keeps it, and ours is dropped.
                    let mut p = self.shared.probe.lock().unwrap();
                    if p.active && !p.done {
                        drop(p);
                        abr.on_probe_dropped();
                        continue;
                    }
                    // Armed before the request leaves: a fast link hands
                    // back the first packets before the next mirror tick.
                    self.session.reset_probe_arrivals();
                    let armed = self.session.stats();
                    *p = ProbeState {
                        active: true,
                        duration_ms,
                        ramp,
                        base_packets: Some(armed.probe_packets_received),
                        base_bytes: Some(armed.probe_bytes_received),
                        ..Default::default()
                    };
                    drop(p);
                    if self
                        .ctrl_tx
                        .try_send(CtrlRequest::Probe(ProbeRequest {
                            target_kbps,
                            duration_ms,
                        }))
                        .is_err()
                    {
                        self.shared.probe.lock().unwrap().active = false; // ctrl queue full — skip
                        abr.on_probe_dropped();
                    }
                }
                Action::AbandonProbe => self.shared.probe.lock().unwrap().active = false,
            }
        }
        request_kbps
    }

    /// The report window closed: publish it for the embedder, then run the
    /// standing-latency ladder and the per-window logs.
    pub(super) fn close_window(
        &mut self,
        lp: &mut PumpLoop,
        window: crate::abr::ClosedWindow,
        request_kbps: Option<u32>,
    ) {
        if !window.discarded {
            self.shared
                .draining
                .store(window.sample.sock_drops > 0, Ordering::Relaxed);
        }
        let abr = &lp.abr;
        // Published at the first window, not the moment the ramp
        // stopped: the rate it opened at is the one the host acked,
        // and that ack is still in flight while the ramp finishes.
        if let Some(outcome) = abr.ramp_outcome() {
            let mut slot = self
                .shared
                .abr_ramp
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some(crate::abr::RampRecord {
                    steps: abr.ramp_steps().to_vec(),
                    outcome,
                    opening_kbps: abr.target_kbps(),
                });
            }
        }
        {
            let mut q = self
                .shared
                .abr_windows
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if q.len() == ABR_TRAJECTORY_WINDOWS {
                q.pop_front();
            }
            q.push_back(crate::abr::WindowRecord {
                t_ms: window
                    .sample
                    .now
                    .duration_since(lp.session_start)
                    .as_millis() as u64,
                // The rate the window ran at: the ask has not been
                // acked yet, so it is not this window's rate.
                rate_kbps: abr.target_kbps(),
                request_kbps,
                sample: window.sample,
                discarded: window.discarded,
                reason: abr.reason(),
            });
        }
        // No-op clock flush suspected a wall-clock step: re-sync
        // once. The 60 s periodic covers everything else.
        if lp.jump.take_resync() {
            let _ = self.ctrl_tx.try_send(CtrlRequest::ClockResync);
        }
        // Standing-latency window close. Escalation: re-sync (stale
        // offset), then bleed (flush+keyframe), then disarm (path
        // latency changed).
        match lp.standing_lat.on_window(window.loss_free) {
            StandingLatAction::None => {}
            StandingLatAction::Resync { above_ms } => {
                tracing::info!(
                    above_ms,
                    "standing latency above the session floor with zero loss — \
                     requesting a clock re-sync first (a stale offset reads exactly \
                     like this)"
                );
                let _ = self.ctrl_tx.try_send(CtrlRequest::ClockResync);
            }
            // Shares the jump-to-live cooldown. An unexecuted bleed re-arms
            // as the detector's run rebuilds.
            StandingLatAction::Bleed { above_ms } => {
                if lp.jump.claim_flush(Instant::now()) {
                    // Not a jump-to-live: that is ABR SEVERE (immediate
                    // ×0.7). Bleed fires after ~6 clean windows with a
                    // sub-25 ms elevation the controller already scores as fine.
                    let (flushed, dropped) = self.shed_backlog();
                    lp.standing_lat.bled();
                    tracing::warn!(
                        above_ms,
                        flushed_datagrams = flushed,
                        dropped_frames = dropped,
                        "standing latency survived a clock re-sync — bled the local \
                         backlog (flush + keyframe)"
                    );
                }
            }
            StandingLatAction::Disarm { above_ms } => {
                tracing::warn!(
                    above_ms,
                    "standing latency persists after a re-sync and every bleed — not \
                     local, not clock; the path latency changed. Leaving it be \
                     (reconnect re-baselines)"
                );
            }
        }
        // The overlay names why Automatic sits low; the cut
        // stands until the host grants a climb.
        self.shared.rate_cut.store(
            lp.abr
                .last_cut()
                .and_then(crate::hud::RateCut::of_reason)
                .map_or(0, |c| c as u8),
            Ordering::Relaxed,
        );
        self.shared
            .quality_floor_kbps
            .store(lp.abr.quality_floor().unwrap_or(0), Ordering::Relaxed);
        if let Some(perf) = lp.perf.as_mut() {
            if let Some(p) = self.session.take_pump_perf() {
                let per_pkt_ns = |ns: u64| ns.checked_div(p.packets).unwrap_or(0);
                tracing::info!(
                    recv_ms = p.recv_ns / 1_000_000,
                    decrypt_ms = p.decrypt_ns / 1_000_000,
                    reasm_ms = p.reasm_ns / 1_000_000,
                    packets = p.packets,
                    placed = p.placed,
                    batches = p.batches,
                    pkts_per_batch = p.packets.checked_div(p.batches).unwrap_or(0),
                    decrypt_ns_pkt = per_pkt_ns(p.decrypt_ns),
                    reasm_ns_pkt = per_pkt_ns(p.reasm_ns),
                    "pump stage split (window)"
                );
            }
            perf.log_and_clear();
        }
    }
}

/// A positive `u32` from the environment. Unset, zero or garbage is `None`.
fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&v| v > 0)
}

/// The control task's events since the last iteration, folded the way the pump applies them.
#[derive(Debug, Default, PartialEq)]
struct Drained {
    /// One re-sync re-bases the latency floors however many landed.
    resynced: bool,
    /// The newest gap: one discard per window.
    gap_ms: Option<u32>,
    /// The newest mode, applied once.
    mode: Option<crate::config::Mode>,
    /// Every ack in arrival order: host-cap learning needs two consecutive short acks in one
    /// window, so a full resolve ack must not clobber a corrective short one.
    acks: Vec<(u32, Option<crate::quic::AckReason>)>,
}

fn drain_events(rx: &std::sync::mpsc::Receiver<PumpEvent>) -> Drained {
    let mut d = Drained::default();
    for ev in rx.try_iter() {
        match ev {
            PumpEvent::ClockResynced => d.resynced = true,
            PumpEvent::BitrateAck(kbps, why) => d.acks.push((kbps, why)),
            PumpEvent::PipelineGap(ms) => d.gap_ms = Some(ms),
            PumpEvent::Mode(m) => d.mode = Some(m),
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_fold_to_what_the_pump_applies_once() {
        let mode = |width| crate::config::Mode {
            width,
            height: 720,
            refresh_hz: 60,
        };
        let short = Some(crate::quic::AckReason::EncoderLimit);
        let (tx, rx) = std::sync::mpsc::channel();
        assert_eq!(drain_events(&rx), Drained::default(), "an idle session");
        for ev in [
            PumpEvent::PipelineGap(401),
            PumpEvent::BitrateAck(8_000, short),
            PumpEvent::Mode(mode(1280)),
            PumpEvent::ClockResynced,
            PumpEvent::BitrateAck(20_000, None),
            PumpEvent::ClockResynced,
            PumpEvent::Mode(mode(1920)),
            PumpEvent::PipelineGap(120),
        ] {
            tx.send(ev).unwrap();
        }
        let want = Drained {
            resynced: true,
            gap_ms: Some(120),
            mode: Some(mode(1920)),
            acks: vec![(8_000, short), (20_000, None)],
        };
        assert_eq!(drain_events(&rx), want);
        // Taken once: the gap discards one window, not every later one.
        assert_eq!(drain_events(&rx), Drained::default());
    }
}
