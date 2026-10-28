//! The `kscreen-doctor` half of KWin output management.
//!
//! Each call shells out to `kscreen-doctor`, bounded by [`KSCREEN_BUDGET`]: it is a Wayland
//! client of the compositor it configures, so a wedged KWin blocks it in connect. Outputs are
//! addressed by the kscreen id [`resolve_kscreen_addr`] finds, never by a name a superseded
//! sibling still carries. Modes are selected by kscreen id ([`CVT_H_GRANULARITY`]).
//!
//! The JSON readers are split from the process calls so their tests run on captured
//! `kscreen-doctor -j` documents.

use super::{mode_satisfies, CVT_H_GRANULARITY, MANAGED_PREFIX};
use std::time::Duration;

/// Re-enable outputs `exclusive` disabled, via `kscreen-doctor` — fallback when
/// [`crate::kwin::mgmt::reenable_outputs`] reports the compositor did not answer.
/// The registry runs this when the group's last member drops, before reclaiming that
/// member's output, so KWin never sees zero enabled outputs.
///
/// A helper killed at [`KSCREEN_BUDGET`] is not a refusal: kscreen-doctor applies, then
/// waits on the compositor, so a slow KWin can land the enable and still get killed.
/// Treat `None` as "unknown" and continue to the mode re-assert and the settle.
pub(super) fn reenable_outputs_kscreen(outputs: &[(String, String)]) {
    if outputs.is_empty() {
        return;
    }
    // Enable first, alone: a bare `output.X.enable` always succeeds. Batching a stale
    // `mode` into the same invoke can reject the whole config and leave the output dark.
    let enable_args: Vec<String> = outputs
        .iter()
        .map(|(name, _)| format!("output.{name}.enable"))
        .collect();
    let enable_verdict = kscreen_verdict(&enable_args);
    match enable_verdict {
        // Both in-process and this path declined. Stop: further mode/settle work cannot
        // light a refused enable.
        Some(false) => {
            tracing::error!(
                outputs = ?outputs,
                args = ?enable_args,
                "KWin: the physical/bootstrap outputs did not re-enable (kscreen-doctor refused \
                 the config or would not run, after the in-process restore already declined) — \
                 a monitor may be left dark"
            );
            return;
        }
        // Budget kill ≠ refusal. kscreen-doctor applies then waits on KWin, so a loaded
        // compositor can land the enable and still get killed. Returning here would skip
        // the mode re-assert (120 Hz comes back at EDID ~60 Hz) and the 200 ms settle
        // that keeps KWin from seeing zero outputs when the caller reclaims next.
        None => tracing::warn!(
            outputs = ?outputs,
            args = ?enable_args,
            "KWin: kscreen-doctor was killed at its budget re-enabling the physical/bootstrap \
             outputs — the apply may well have landed, so continuing with the mode restore"
        ),
        Some(true) => {}
    }
    // Then re-assert each captured mode. A bare enable falls back to EDID-preferred
    // (~60 Hz on a 120 Hz panel). A rejected mode is a wrong refresh, not a dark screen.
    let mode_args: Vec<String> = outputs
        .iter()
        .filter(|(_, mode)| !mode.is_empty())
        .map(|(name, mode)| format!("output.{name}.mode.{mode}"))
        .collect();
    let modes_restored = mode_args.is_empty() || kscreen_ok(&mode_args);
    std::thread::sleep(Duration::from_millis(200));
    // After a budget kill the enable is probable, not established; the log must say which.
    let enable_confirmed = enable_verdict == Some(true);
    if modes_restored {
        tracing::info!(reenabled = ?outputs, enable_confirmed, "KWin: restored the physical/bootstrap outputs at their captured modes (group empty)");
    } else {
        tracing::warn!(
            reenabled = ?outputs,
            args = ?mode_args,
            enable_confirmed,
            "KWin: re-enabled the physical/bootstrap outputs but could not re-assert their captured \
             modes — they are back at KWin's preferred refresh, not the one they were streaming at"
        );
    }
}

/// kscreen address of the output just created ([`newest_id_at`]), falling back to
/// `Virtual-<name>`. The name is ambiguous during a supersede (the replacement reuses the
/// per-slot name while the sibling is still alive).
pub(super) fn resolve_kscreen_addr(name: &str, w: u32, h: u32) -> String {
    let fallback = format!("Virtual-{name}");
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(150));
        }
        let Some(doc) = kscreen_json() else { continue };
        if let Some(id) = newest_id_at(&doc, &fallback, w, h) {
            tracing::info!(id, name, w, h, "KWin: resolved the new output's kscreen id");
            return id.to_string();
        }
    }
    tracing::warn!(
        name,
        w,
        h,
        "KWin: could not resolve the new output's kscreen id — falling back to name addressing \
         (ambiguous during a mode-switch supersede)"
    );
    fallback
}

/// Highest kscreen id of an output named exactly `name` at `(w, h)`: only the new output
/// sits at the size it was born at, and kscreen ids grow. Slot 1 never matches slot 12.
fn newest_id_at(doc: &serde_json::Value, name: &str, w: u32, h: u32) -> Option<u64> {
    doc.get("outputs")?
        .as_array()?
        .iter()
        .filter(|o| {
            o.get("name").and_then(|n| n.as_str()) == Some(name)
                && output_active_size(o) == Some((w, h))
        })
        .filter_map(|o| o.get("id").and_then(|i| i.as_u64()))
        .max()
}

/// Budget for one `kscreen-doctor` call. It is a Wayland client of the compositor it
/// configures, so a wedged KWin blocks in connect forever — and these calls run on
/// the session's stream thread. Healthy calls are tens of ms.
const KSCREEN_BUDGET: Duration = Duration::from_secs(5);

/// `kscreen-doctor <args>` for exit status, bounded by [`KSCREEN_BUDGET`]. A timeout
/// is a failed apply.
fn kscreen_ok(args: &[String]) -> bool {
    kscreen_verdict(args) == Some(true)
}

/// Place `output` (a resolved kscreen address) at `(x, y)` in the desktop layout.
pub(super) fn set_position(output: &str, x: i32, y: i32) -> bool {
    let ok = kscreen_ok(&[format!("output.{output}.position.{x},{y}")]);
    if ok {
        tracing::info!(output, x, y, "KWin: placed output in the desktop layout");
    } else {
        tracing::warn!(output, x, y, "KWin: output position apply failed");
    }
    ok
}

/// Same call, keeping the outcome [`kscreen_ok`]'s `bool` throws away.
///
/// `Some(true)`/`Some(false)`: ran to completion and accepted / refused (spawn failure
/// is a refusal). `None`: killed at [`KSCREEN_BUDGET`]. kscreen-doctor applies then
/// waits on the compositor, so a slow KWin yields a kill on a request that already
/// landed; treating `None` as failure costs a monitor its refresh on the restore path.
pub(crate) fn kscreen_verdict(args: &[String]) -> Option<bool> {
    // kscreen-doctor is a Wayland client: it gets the live session's socket.
    let mut doctor = std::process::Command::new("kscreen-doctor");
    doctor.args(args);
    match crate::proc::status_within(&mut doctor, KSCREEN_BUDGET) {
        Ok(status) => Some(status.success()),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => None,
        Err(_) => Some(false),
    }
}

fn kscreen_json_bytes() -> Option<Vec<u8>> {
    let mut doctor = std::process::Command::new("kscreen-doctor");
    doctor.arg("-j");
    crate::proc::output_within(&mut doctor, KSCREEN_BUDGET)
        .ok()
        .map(|o| o.stdout)
}

fn kscreen_json() -> Option<serde_json::Value> {
    serde_json::from_slice(&kscreen_json_bytes()?).ok()
}

/// Current mode from a `kscreen-doctor -j` entry: `(width, height, refresh_mHz)`.
/// No current mode or no size → `None`. Missing `refreshRate` reports 0 mHz.
fn output_active_mode(o: &serde_json::Value) -> Option<(u32, u32, u32)> {
    let current = o.get("currentModeId").and_then(json_id)?;
    let mode = o
        .get("modes")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(json_id).as_deref() == Some(current.as_str()))?;
    let size = mode.get("size")?;
    let w = size.get("width").and_then(|v| v.as_u64())? as u32;
    let h = size.get("height").and_then(|v| v.as_u64())? as u32;
    // Hz → mHz without an intermediate round: `refreshRate` is a float (59.94, 119.92).
    // Whole Hz would throw away the distinction `PhysicalMonitor::refresh_mhz` keeps.
    let mhz = mode
        .get("refreshRate")
        .and_then(|r| r.as_f64())
        .map(|hz| (hz * 1000.0).round().max(0.0) as u32)
        .unwrap_or(0);
    Some((w, h, mhz))
}

fn output_active_size(o: &serde_json::Value) -> Option<(u32, u32)> {
    output_active_mode(o).map(|(w, h, _)| (w, h))
}

/// Parse `kscreen-doctor -j` into the shared monitor type. Split from the process
/// call so the mapping can be tested against captured JSON. Mirrors the in-process
/// contract: disabled outputs report zeroed geometry, `primary` accepts `priority: 1`
/// or `primary: true`, list sorted by desktop position.
fn monitors_from_kscreen_json(doc: &serde_json::Value) -> Vec<crate::monitors::PhysicalMonitor> {
    let Some(outputs) = doc.get("outputs").and_then(|o| o.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<crate::monitors::PhysicalMonitor> = outputs
        .iter()
        .filter_map(|o| {
            let connector = o.get("name").and_then(|n| n.as_str())?.to_string();
            let mode = output_active_mode(o);
            let coord = |k: &str| {
                o.get("pos")
                    .and_then(|p| p.get(k))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32
            };
            Some(crate::monitors::PhysicalMonitor {
                managed: connector.starts_with(MANAGED_PREFIX),
                description: crate::monitors::describe(
                    o.get("vendor").and_then(|v| v.as_str()).unwrap_or(""),
                    o.get("model").and_then(|v| v.as_str()).unwrap_or(""),
                    &connector,
                ),
                width: mode.map(|m| m.0).unwrap_or(0),
                height: mode.map(|m| m.1).unwrap_or(0),
                refresh_mhz: mode.map(|m| m.2).unwrap_or(0),
                x: coord("x"),
                y: coord("y"),
                scale: o
                    .get("scale")
                    .and_then(|v| v.as_f64())
                    .filter(|s| *s > 0.0)
                    .unwrap_or(1.0),
                primary: o.get("primary").and_then(|p| p.as_bool()).unwrap_or(false)
                    || o.get("priority").and_then(|p| p.as_u64()) == Some(1),
                enabled: o.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false),
                connector,
            })
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    out
}

/// Every head `kscreen-doctor -j` reports; `None` when it does not answer.
pub(super) fn list_monitors() -> Option<Vec<crate::monitors::PhysicalMonitor>> {
    Some(monitors_from_kscreen_json(&kscreen_json()?))
}

#[derive(Clone, Debug, PartialEq)]
struct KModeRow {
    /// kscreen mode id — address by this, never the requested `WxH@Hz` string.
    id: String,
    w: u32,
    h: u32,
    hz: f64,
}

/// A kscreen JSON id, which is a string on some KWin versions and a number on others.
fn json_id(v: &serde_json::Value) -> Option<String> {
    v.as_str()
        .map(|s| s.to_string())
        .or_else(|| v.as_u64().map(|n| n.to_string()))
}

/// Full mode list of `output` (resolved kscreen address) from parsed `-j` JSON.
/// Split from the process call so the picker can be tested on captured JSON.
fn modes_from_json(doc: &serde_json::Value, output: &str) -> Vec<KModeRow> {
    let Some(o) = doc
        .get("outputs")
        .and_then(|v| v.as_array())
        .and_then(|outs| {
            outs.iter().find(|o| {
                o.get("name").and_then(|n| n.as_str()) == Some(output)
                    || o.get("id").and_then(json_id).as_deref() == Some(output)
            })
        })
    else {
        return Vec::new();
    };
    o.get("modes")
        .and_then(|m| m.as_array())
        .map(|ms| {
            ms.iter()
                .filter_map(|m| {
                    let size = m.get("size")?;
                    Some(KModeRow {
                        id: m.get("id").and_then(json_id)?,
                        w: size.get("width").and_then(|v| v.as_u64())? as u32,
                        h: size.get("height").and_then(|v| v.as_u64())? as u32,
                        hz: m.get("refreshRate").and_then(|r| r.as_f64())?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn output_modes(output: &str) -> Vec<KModeRow> {
    kscreen_json()
        .map(|doc| modes_from_json(&doc, output))
        .unwrap_or_default()
}

/// Mode in `modes` that fulfils `width`×`height`@`hz`, allowing CVT alignment
/// ([`CVT_H_GRANULARITY`]): exact height, width up to one cell narrower (never
/// wider), refresh within 1 Hz (excludes the native 60 Hz entry). Widest then
/// fastest, so an exact-width mode beats an aligned one.
fn pick_custom_mode(modes: &[KModeRow], width: u32, height: u32, hz: u32) -> Option<&KModeRow> {
    modes
        .iter()
        .filter(|m| {
            m.h == height
                && m.w <= width
                && width - m.w < CVT_H_GRANULARITY
                && (m.hz - f64::from(hz)).abs() < 1.0
        })
        .max_by(|a, b| {
            a.w.cmp(&b.w)
                .then(a.hz.partial_cmp(&b.hz).unwrap_or(std::cmp::Ordering::Equal))
        })
}

/// Install + select the `width`×`height`@`hz` custom mode via `kscreen-doctor`
/// (`output` is the resolved kscreen address), then read back the active mode as
/// `(width, height, refresh_hz)`. `None` if read-back failed.
///
/// The apply can report success yet leave the old mode; the caller drives the
/// pipeline off the achieved mode. Select by kscreen mode id, never the requested
/// `WxH@Hz` string ([`CVT_H_GRANULARITY`]).
pub(super) fn set_custom_refresh(
    width: u32,
    height: u32,
    hz: u32,
    output: &str,
) -> Option<(u32, u32, u32)> {
    let output = output.to_string();
    let mhz = hz.saturating_mul(1000);
    let run = |arg: String| kscreen_ok(&[arg]);
    // Install only if no usable mode exists: kscreen-doctor appends, and KWin persists
    // the list per output name, so re-adding every connect would grow the display list.
    let mut modes = output_modes(&output);
    if pick_custom_mode(&modes, width, height, hz).is_none() {
        let _ = run(format!(
            "output.{output}.addCustomMode.{width}.{height}.{mhz}.full"
        ));
        modes = output_modes(&output);
    }
    let applied = match pick_custom_mode(&modes, width, height, hz) {
        Some(target) => {
            if (target.w, target.h) != (width, height) {
                tracing::info!(
                    output,
                    requested_w = width,
                    requested_h = height,
                    mode_w = target.w,
                    mode_h = target.h,
                    mode_hz = target.hz,
                    "KWin aligned the custom mode to the CVT cell grain — streaming at its size"
                );
            }
            // By id first; the human `WxH@Hz` (from the mode's own size, not the request)
            // is fallback for builds whose ids don't round-trip through the CLI.
            run(format!("output.{output}.mode.{}", target.id))
                || run(format!(
                    "output.{output}.mode.{}x{}@{}",
                    target.w,
                    target.h,
                    target.hz.round() as u32
                ))
        }
        None => {
            tracing::warn!(
                output,
                requested_w = width,
                requested_h = height,
                requested_hz = hz,
                offered = ?modes,
                "KWin offers no mode matching the request after addCustomMode — is kscreen-doctor \
                 up to date, and KWin ≥ 6.6 (custom modes on virtual outputs)?"
            );
            false
        }
    };
    match read_active_mode(&output) {
        Some((w, h, achieved)) => {
            if !mode_satisfies((w, h), width, height) {
                tracing::warn!(
                    output,
                    requested_w = width,
                    requested_h = height,
                    active_w = w,
                    active_h = h,
                    achieved,
                    applied,
                    "KWin kept a mode that is not the one selected — a stored \
                     kwinoutputconfig.json slot for this output name, not a CVT alignment"
                );
            } else if achieved >= hz {
                tracing::info!(
                    output,
                    requested = hz,
                    achieved,
                    active_w = w,
                    active_h = h,
                    "KWin virtual output: custom mode applied"
                );
            } else {
                tracing::warn!(
                    output,
                    requested = hz,
                    achieved,
                    active_w = w,
                    active_h = h,
                    applied,
                    "KWin virtual output mode below requested — pacing the encoder to the \
                     achieved rate (custom-mode install rejected? is kscreen-doctor up to date?)"
                );
            }
            Some((w, h, achieved.max(1)))
        }
        None => {
            tracing::warn!(
                output,
                requested = hz,
                applied,
                "could not read back KWin virtual output refresh — assuming 60 Hz (is \
                 kscreen-doctor installed?)"
            );
            None
        }
    }
}

/// Active mode `(width, height, refresh_hz)` of `output` (resolved kscreen address)
/// from `kscreen-doctor -j`. Mode/output ids are strings or numbers depending on
/// KWin version; both are accepted.
fn read_active_mode(output: &str) -> Option<(u32, u32, u32)> {
    let doc = kscreen_json()?;
    let as_id = |v: &serde_json::Value| -> Option<String> {
        v.as_str()
            .map(|s| s.to_string())
            .or_else(|| v.as_u64().map(|n| n.to_string()))
    };
    let o = doc.get("outputs")?.as_array()?.iter().find(|o| {
        o.get("name").and_then(|n| n.as_str()) == Some(output)
            || o.get("id").and_then(as_id).as_deref() == Some(output)
    })?;
    let current = o.get("currentModeId").and_then(as_id)?;
    let mode = o
        .get("modes")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(as_id).as_deref() == Some(current.as_str()))?;
    let size = mode.get("size")?;
    let w = size.get("width").and_then(|v| v.as_u64())? as u32;
    let h = size.get("height").and_then(|v| v.as_u64())? as u32;
    let hz = mode.get("refreshRate").and_then(|r| r.as_f64())?;
    Some((w, h, hz.round() as u32))
}

/// Current mode as a kscreen-doctor setter. Prefer human `WxH@Hz` (survives mode-id
/// re-enumeration across disable→enable); fall back to raw `currentModeId`.
fn output_current_mode_spec(o: &serde_json::Value) -> Option<String> {
    let as_id = |v: &serde_json::Value| -> Option<String> {
        v.as_str()
            .map(|s| s.to_string())
            .or_else(|| v.as_u64().map(|n| n.to_string()))
    };
    let current = o.get("currentModeId").and_then(&as_id)?;
    let mode = o
        .get("modes")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(&as_id).as_deref() == Some(current.as_str()))?;
    let human = (|| {
        let size = mode.get("size")?;
        let w = size.get("width").and_then(|v| v.as_u64())?;
        let h = size.get("height").and_then(|v| v.as_u64())?;
        let hz = mode.get("refreshRate").and_then(|r| r.as_f64())?.round() as u64;
        Some(format!("{w}x{h}@{hz}"))
    })();
    Some(human.unwrap_or(current))
}

/// Currently-enabled outputs that are not ours — bootstrap + physical, i.e. what
/// `exclusive` must disable — each paired with its current mode so teardown restores
/// that refresh (a bare enable drops 120 Hz to ~60). Excludes the whole
/// [`MANAGED_PREFIX`] family so a second exclusive session never disables the first.
fn other_enabled_outputs() -> Vec<(String, String)> {
    let out = match kscreen_json_bytes() {
        Some(o) => o,
        None => return Vec::new(),
    };
    let doc: serde_json::Value = match serde_json::from_slice(&out) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    doc.get("outputs")
        .and_then(|o| o.as_array())
        .map(|outs| {
            outs.iter()
                .filter(|o| o.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false))
                .filter_map(|o| {
                    let name = o.get("name").and_then(|n| n.as_str())?;
                    (!name.starts_with(MANAGED_PREFIX)).then(|| {
                        (
                            name.to_string(),
                            output_current_mode_spec(o).unwrap_or_default(),
                        )
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// True if any managed group member is already KWin primary — first-slot-wins, so a
/// later exclusive session doesn't steal the shell. No primary flag → `false` (we then
/// set ourselves). Accepts `"priority": 1` or `"primary": true`.
fn a_managed_output_is_primary() -> bool {
    let Some(out) = kscreen_json_bytes() else {
        return false;
    };
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&out) else {
        return false;
    };
    doc.get("outputs")
        .and_then(|o| o.as_array())
        .map(|outs| {
            outs.iter().any(|o| {
                let managed = o
                    .get("name")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| n.starts_with(MANAGED_PREFIX));
                let primary = o.get("primary").and_then(|p| p.as_bool()).unwrap_or(false)
                    || o.get("priority").and_then(|p| p.as_u64()) == Some(1);
                managed && primary
            })
        })
        .unwrap_or(false)
}

/// Set our output primary and disable bootstrap/physicals so the managed group is the
/// sole desktop. `ours` is the resolved kscreen address. `journal` gets the outputs to
/// disable before the disable. On failure, streaming continues rather than failing the session.
pub(super) fn apply_virtual_primary(ours: &str, mut journal: impl FnMut(&[(String, String)])) {
    let kscreen = |args: &[String]| kscreen_ok(args);
    // First-slot-wins: only grab primary if no managed member has it, so a second
    // exclusive session joins as a secondary instead of stealing the shell.
    let sole = !a_managed_output_is_primary();
    if sole {
        if !kscreen(&[format!("output.{ours}.primary")]) {
            tracing::warn!(
                "KWin: virtual output not made primary — the client may see only the wallpaper"
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // Disable still-enabled non-managed outputs (bootstrap / physical), but the ones the
    // operator keeps lit, as the in-process path does. Capture each with its current mode
    // so teardown restores the real refresh.
    let keep = crate::policy::prefs().get().keep_monitors;
    let others: Vec<_> = other_enabled_outputs()
        .into_iter()
        .filter(|(name, _)| crate::monitors::darkens(name, true, false, false, &keep))
        .collect();
    let mut args: Vec<String> = others
        .iter()
        .map(|(o, _mode)| format!("output.{o}.disable"))
        .collect();
    // Ours ends up the whole desktop, so it belongs at the desktop origin: KWin parks
    // a new output right of the existing row and never re-normalizes once the row goes
    // dark, and Plasma places popups off a screen whose origin is not zero.
    if sole {
        args.push(format!("output.{ours}.position.0,0"));
    }
    journal(&others);
    if args.is_empty() {
        return;
    }
    if kscreen(&args) {
        tracing::info!(also_disabled = ?others, "KWin: streamed output set as the sole desktop");
    } else {
        // Report the request, not success: the outputs may still be enabled. They stay
        // journaled: re-enabling a lit output is a no-op, and a disable that landed with
        // its ack lost to the budget would otherwise strand a physical.
        tracing::warn!(
            attempted_disable = ?others,
            "KWin: could not disable the other outputs for the exclusive topology (kscreen-doctor \
             failed or hit its budget) — the streamed output is not the sole desktop"
        );
    }
}

/// Make the streamed output primary but keep other outputs enabled. Nothing to restore.
pub(super) fn apply_virtual_primary_only(ours: &str) {
    let ok = kscreen_ok(&[format!("output.{ours}.primary")]);
    if ok {
        tracing::info!("KWin: streamed output set primary (physical outputs kept)");
    } else {
        tracing::warn!("KWin: virtual output not made primary");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, w: u32, h: u32, hz: f64) -> KModeRow {
        KModeRow {
            id: id.to_string(),
            w,
            h,
            hz,
        }
    }

    /// libxcvt rounds 2868×1320@120 to 2864×1320@119.92. Selecting by the requested
    /// `WxH@Hz` string matches nothing; the picker must find the aligned mode.
    #[test]
    fn picks_the_cvt_aligned_mode() {
        let modes = [
            row("1", 2868, 1320, 60.0),   // the virtual output's native/birth mode
            row("2", 2864, 1320, 119.92), // the custom mode KWin actually generated
        ];
        let got = pick_custom_mode(&modes, 2868, 1320, 120).expect("CVT-aligned mode");
        assert_eq!((got.id.as_str(), got.w, got.h), ("2", 2864, 1320));
    }

    /// A width already on the cell grain round-trips; an exact-width mode outranks
    /// an aligned one when both are offered.
    #[test]
    fn exact_width_outranks_an_aligned_one() {
        let modes = [
            row("1", 2560, 1440, 60.0),
            row("2", 2552, 1440, 119.93), // a stale narrower custom mode from an earlier session
            row("3", 2560, 1440, 119.98),
        ];
        let got = pick_custom_mode(&modes, 2560, 1440, 120).expect("exact mode");
        assert_eq!(got.id, "3");
    }

    /// Never wander onto the native 60 Hz entry, a different height, a wider width,
    /// or a mode more than one cell narrower than asked.
    #[test]
    fn rejects_modes_that_are_not_the_request() {
        let modes = [
            row("1", 2868, 1320, 60.0),   // native — refresh too far off
            row("2", 2868, 1080, 119.92), // wrong height
            row("3", 2880, 1320, 119.92), // wider than requested
            row("4", 2856, 1320, 119.92), // two cells narrower — not a CVT alignment of 2868
            row("5", 1920, 1080, 120.0),  // unrelated
        ];
        assert!(pick_custom_mode(&modes, 2868, 1320, 120).is_none());
    }

    /// Mode + output ids are JSON strings on some KWin versions and numbers on others;
    /// both must parse. A row missing size/refresh is skipped, not a poisoned list.
    #[test]
    fn parses_both_id_encodings() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"outputs":[
                 {"id":7,"name":"Virtual-punktfunk","modes":[
                   {"id":"m1","size":{"width":2868,"height":1320},"refreshRate":60.0},
                   {"id":42,"size":{"width":2864,"height":1320},"refreshRate":119.92},
                   {"id":"broken","size":{"width":800}}
                 ]},
                 {"id":1,"name":"eDP-1","modes":[
                   {"id":"x","size":{"width":2864,"height":1320},"refreshRate":119.92}
                 ]}
               ]}"#,
        )
        .expect("fixture parses");
        // Addressable by numeric id (how `resolve_kscreen_addr` returns it) and by name.
        for addr in ["7", "Virtual-punktfunk"] {
            let modes = modes_from_json(&doc, addr);
            assert_eq!(modes.len(), 2, "the malformed row is dropped ({addr})");
            assert_eq!(modes[1].id, "42", "numeric mode ids stringify ({addr})");
            let got = pick_custom_mode(&modes, 2868, 1320, 120).expect("aligned mode");
            assert_eq!(got.id, "42");
        }
        // Never reads another output's list (the eDP-1 entry carries a matching mode).
        assert!(modes_from_json(&doc, "Virtual-nope").is_empty());
    }

    /// kscreen fallback for `monitors::list` matches the in-process contract: geometry
    /// from `pos`, mode in pixels with refresh in mHz (59.94 ≠ 60), a disabled head
    /// listed but zeroed, our managed output flagged, list sorted by position.
    #[test]
    fn parses_a_kscreen_monitor_list() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"outputs":[
                 {"id":2,"name":"HDMI-A-1","enabled":true,"priority":2,"scale":1,
                  "pos":{"x":1920,"y":0},"vendor":"ACME","model":"U2720Q",
                  "currentModeId":"m9","modes":[
                    {"id":"m9","size":{"width":1920,"height":1080},"refreshRate":59.94}]},
                 {"id":1,"name":"eDP-1","enabled":true,"priority":1,"scale":1.5,
                  "pos":{"x":0,"y":0},
                  "currentModeId":7,"modes":[
                    {"id":7,"size":{"width":3840,"height":2160},"refreshRate":120.0}]},
                 {"id":3,"name":"DP-3","enabled":false,"scale":1,"pos":{"x":0,"y":0},
                  "modes":[{"id":"z","size":{"width":2560,"height":1440},"refreshRate":60.0}]},
                 {"id":4,"name":"Virtual-punktfunk-7","enabled":true,"scale":1,
                  "pos":{"x":5760,"y":0},"currentModeId":"v1","modes":[
                    {"id":"v1","size":{"width":2560,"height":1440},"refreshRate":119.98}]}
               ]}"#,
        )
        .expect("fixture parses");
        let mons = monitors_from_kscreen_json(&doc);
        let by = |c: &str| {
            mons.iter()
                .find(|m| m.connector == c)
                .unwrap_or_else(|| panic!("{c} missing"))
                .clone()
        };
        // Sorted by desktop position, not by kscreen's own order.
        let order: Vec<&str> = mons.iter().map(|m| m.connector.as_str()).collect();
        assert_eq!(
            order,
            vec!["DP-3", "eDP-1", "HDMI-A-1", "Virtual-punktfunk-7"]
        );
        let edp = by("eDP-1");
        // Pixels at the scale the desk runs — the point of `logical_size`.
        assert_eq!((edp.width, edp.height), (3840, 2160));
        assert_eq!(edp.scale, 1.5);
        assert_eq!(edp.logical_size(), (2560.0, 1440.0));
        assert!(edp.primary, "priority 1 is KWin's primary");
        assert_eq!(edp.refresh_mhz, 120_000);
        // 59.94 must survive as mHz; rounding to whole Hz here is the bug this guards.
        assert_eq!(by("HDMI-A-1").refresh_mhz, 59_940);
        assert_eq!(by("HDMI-A-1").description, "ACME U2720Q");
        assert!(!by("HDMI-A-1").primary);
        // Disabled: listed with no invented mode.
        let dark = by("DP-3");
        assert!(!dark.enabled);
        assert_eq!((dark.width, dark.height, dark.refresh_mhz), (0, 0, 0));
        // Ours, labelled by connector when the entry carries no make/model.
        let ours = by("Virtual-punktfunk-7");
        assert!(ours.managed);
        assert_eq!(ours.description, "Virtual-punktfunk-7");
        assert!(!by("eDP-1").managed);
    }

    /// No `outputs` array is an empty list, never a panic.
    #[test]
    fn a_malformed_kscreen_document_yields_no_monitors() {
        assert!(monitors_from_kscreen_json(&serde_json::json!({})).is_empty());
        assert!(monitors_from_kscreen_json(&serde_json::json!({"outputs": 7})).is_empty());
        // An output with no name cannot be pinned, so it is dropped.
        assert!(
            monitors_from_kscreen_json(&serde_json::json!({"outputs": [{"enabled": true}]}))
                .is_empty()
        );
    }

    /// Slot 1's create resolves its own output, never slot 12's at the same size, and the
    /// newest of two same-named outputs at its birth size.
    #[test]
    fn the_new_output_resolves_by_exact_name_and_birth_size() {
        let at = |id: u64, name: &str, w: u32| {
            serde_json::json!({"id": id, "name": name, "currentModeId": "m",
                "modes": [{"id": "m", "size": {"width": w, "height": 1096}, "refreshRate": 60.0}]})
        };
        let doc = serde_json::json!({"outputs": [
            at(3, "Virtual-punktfunk-1", 1920),
            at(5, "Virtual-punktfunk-1", 2560),
            at(9, "Virtual-punktfunk-12", 1920),
            at(4, "Virtual-punktfunk-1", 1920),
        ]});
        assert_eq!(
            newest_id_at(&doc, "Virtual-punktfunk-1", 1920, 1096),
            Some(4)
        );
        assert_eq!(newest_id_at(&doc, "Virtual-punktfunk", 1920, 1096), None);
    }
}
