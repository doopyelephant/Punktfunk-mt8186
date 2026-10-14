//! niri backend: one virtual output per [`create`](VirtualDisplay::create), made over niri's IPC
//! and cast through its in-process ScreenCast ([`cast`]).
//!
//! Only a niri that serves `CreateVirtualOutput` can host — the biri fork today
//! (`design/niri-support.md`). [`ipc::probe`] tells it from stock niri without side effects, and
//! stock niri gets [`NoVirtualOutputs`], which names biri.
//!
//! Outputs are `PF-<pid>-<n>`, minted by the same counter as Hyprland's, so a leftover from a
//! killed host is recognisable and [`reclaim_leftovers_once`] can remove it.

pub(crate) mod cast;
pub(crate) mod ipc;

use super::{Mode, VirtualDisplay, VirtualOutput};
use crate::hyprland::{is_managed_output, next_output_name, output_owner_pid};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant};

/// How long `create` waits for a new output to report its mode. niri applies it in the same
/// event-loop turn; this covers a busy compositor.
const MODE_WAIT: Duration = Duration::from_millis(1500);

/// The running niri has no virtual outputs: stock niri, not biri.
#[derive(Debug)]
pub struct NoVirtualOutputs;

impl std::fmt::Display for NoVirtualOutputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "this niri has no virtual outputs — install biri, the niri fork that adds them. See \
             https://docs.punktfunk.unom.io/docs/niri#get-biri",
        )
    }
}

impl std::error::Error for NoVirtualOutputs {}

/// A niri session is running for this user.
pub(crate) fn is_available() -> bool {
    crate::session::niri_socket().is_some()
}

/// The socket answers, and this niri can create virtual outputs.
pub(crate) fn probe() -> Result<()> {
    let version = ipc::version().unwrap_or_default();
    match ipc::probe().context("ask niri over its IPC socket")? {
        ipc::Capability::VirtualOutputs => {
            tracing::info!(version = %version, "niri: virtual outputs available");
            Ok(())
        }
        ipc::Capability::NoVirtualOutputs => {
            tracing::warn!(version = %version, "{NoVirtualOutputs}");
            Err(NoVirtualOutputs.into())
        }
        ipc::Capability::Unsupported(why) => {
            bail!("niri {version} can't create a virtual output here: {why}")
        }
    }
}

/// Every head niri reports, for the monitor picker and the mirror pin.
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>> {
    let focused = ipc::workspaces()
        .ok()
        .and_then(|ws| ws.into_iter().find(|w| w.is_focused))
        .and_then(|w| w.output);
    let mut out: Vec<_> = ipc::outputs()?
        .into_iter()
        .map(|o| {
            let mode = o.mode().cloned();
            let logical = o.logical.clone();
            crate::monitors::PhysicalMonitor {
                description: crate::monitors::describe(&o.make, &o.model, &o.name),
                width: mode.as_ref().map_or(0, |m| m.width),
                height: mode.as_ref().map_or(0, |m| m.height),
                refresh_mhz: mode.as_ref().map_or(0, |m| m.refresh_rate),
                x: logical.as_ref().map_or(0, |l| l.x),
                y: logical.as_ref().map_or(0, |l| l.y),
                scale: logical
                    .as_ref()
                    .map(|l| l.scale)
                    .filter(|s| *s > 0.0)
                    .unwrap_or(1.0),
                primary: focused.as_deref() == Some(o.name.as_str()),
                enabled: logical.is_some(),
                managed: is_managed_output(&o.name),
                connector: o.name,
            }
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
}

/// Cast a head niri already has (the monitor-mirror pin). Works on stock niri too.
pub(crate) fn stream_existing_output(
    connector: &str,
    hw_cursor: bool,
) -> Result<crate::mirror::MirrorStream> {
    let (node_id, keepalive) = cast::start(connector, hw_cursor)?;
    Ok(crate::mirror::MirrorStream {
        node_id,
        // niri's ScreenCast publishes on the user's own PipeWire daemon.
        remote_fd: None,
        // Not a portal: niri honours the cursor mode it was asked for.
        cursor_mode: None,
        keepalive,
    })
}

/// Point niri's focus at `name`: new windows open on the focused output, and an output nothing
/// draws on produces no frames.
pub(crate) fn focus_output(name: &str) {
    if let Err(e) = ipc::action(json!({ "FocusMonitor": { "output": name } })) {
        tracing::warn!(output = %name, error = %format!("{e:#}"), "niri: output not focused");
    }
}

/// Remove `PF-<pid>-<n>` outputs whose host is gone. Once per process: a SIGKILLed host's
/// outputs outlive it, and nothing else unplugs them.
fn reclaim_leftovers_once() {
    static RECLAIMED: Once = Once::new();
    RECLAIMED.call_once(|| {
        let Ok(outputs) = ipc::outputs() else { return };
        for name in outputs.into_iter().map(|o| o.name) {
            let Some(pid) = output_owner_pid(&name) else {
                continue;
            };
            if pid == std::process::id() || std::path::Path::new(&format!("/proc/{pid}")).exists() {
                continue;
            }
            match ipc::remove_virtual_output(&name) {
                Ok(()) => tracing::info!(
                    output = %name, owner_pid = pid,
                    "niri: removed a virtual output left behind by a host that is no longer running"
                ),
                Err(e) => tracing::warn!(
                    output = %name, owner_pid = pid, error = %format!("{e:#}"),
                    "niri: leftover virtual output not removed"
                ),
            }
        }
    });
}

/// Poll until `name` reports a mode, then return it as `(width, height, millihertz)`.
fn wait_for_mode(name: &str) -> Result<(u32, u32, u32)> {
    let deadline = Instant::now() + MODE_WAIT;
    loop {
        let found = ipc::outputs()?
            .into_iter()
            .find(|o| o.name == name)
            .and_then(|o| o.mode().map(|m| (m.width, m.height, m.refresh_rate)));
        if let Some(mode) = found {
            return Ok(mode);
        }
        if Instant::now() >= deadline {
            bail!("niri created {name} but it reported no mode within {MODE_WAIT:?}");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Removes its output on drop.
struct OutputGuard(String);

impl Drop for OutputGuard {
    fn drop(&mut self) {
        match ipc::remove_virtual_output(&self.0) {
            Ok(()) => tracing::info!(output = %self.0, "niri: virtual output removed"),
            Err(e) => tracing::warn!(
                output = %self.0, error = %format!("{e:#}"),
                "niri: virtual output not removed"
            ),
        }
    }
}

/// A session's output. Field order is drop order: the cast stops before its output goes.
struct OutputKeepalive {
    _cast: Box<dyn Send>,
    _output: OutputGuard,
}

pub struct NiriDisplay {
    hw_cursor: bool,
}

impl NiriDisplay {
    pub fn new() -> Result<Self> {
        Ok(NiriDisplay { hw_cursor: false })
    }
}

impl VirtualDisplay for NiriDisplay {
    fn name(&self) -> &'static str {
        "niri"
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        probe()?;
        reclaim_leftovers_once();
        let name = ipc::create_virtual_output(
            &next_output_name(),
            mode.width,
            mode.height,
            mode.refresh_hz,
        )
        .context("create a niri virtual output")?;
        let output = OutputGuard(name.clone());
        let (width, height, mhz) = wait_for_mode(&name)?;
        if (width, height) != (mode.width, mode.height) {
            tracing::warn!(
                output = %name, want = %format!("{}x{}", mode.width, mode.height),
                got = %format!("{width}x{height}"),
                "niri: the virtual output came up at another size — streaming what it has"
            );
        }
        focus_output(&name);
        let (node_id, cast) = cast::start(&name, self.hw_cursor)?;
        tracing::info!(
            output = %name, node_id, w = width, h = height, mhz,
            "niri virtual output ready"
        );
        let keepalive = OutputKeepalive {
            _cast: cast,
            _output: output,
        };
        let mut out = VirtualOutput::owned(
            node_id,
            Some((width, height, mode.refresh_hz)),
            Box::new(keepalive),
        );
        // Extend topology: input and direct capture find the output by its `wl_output` name.
        out.output_name = Some(name);
        Ok(out)
    }
}
