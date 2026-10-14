//! A PipeWire cast of one niri output through niri's in-process `org.gnome.Mutter.ScreenCast`.
//!
//! niri serves the ScreenCast half of Mutter's API (the `xdp-gnome-screencast` feature, on by
//! default) but not RemoteDesktop, so the session stands alone: `CreateSession({})` →
//! `RecordMonitor(output)` → `Start` → `PipeWireStreamAdded`. The node lives on the user's
//! PipeWire daemon, so there is no remote fd to carry. `hw_cursor` asks for the pointer as
//! metadata; otherwise niri paints it into the frames.

use anyhow::{anyhow, bail, Context, Result};
use ashpd::zbus;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use zbus::zvariant::{OwnedObjectPath, Value};

const BUS_SC: &str = "org.gnome.Mutter.ScreenCast";
/// Same cursor-mode values as Mutter's: 1 paints the pointer, 2 sends it as metadata.
const CURSOR_EMBEDDED: u32 = 1;
const CURSOR_METADATA: u32 = 2;

/// Start a cast of `output`; the node id and the keepalive whose drop stops it.
pub(crate) fn start(output: &str, hw_cursor: bool) -> Result<(u32, Box<dyn Send>)> {
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let output_thread = output.to_string();
    thread::Builder::new()
        .name("punktfunk-niri-cast".into())
        .spawn(move || cast_thread(setup_tx, stop_thread, output_thread, hw_cursor))
        .context("spawn the niri cast thread")?;
    // Built before the wait so a timeout still stops the thread.
    let guard = crate::proc::StopFlag(stop);
    match setup_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(node_id)) => Ok((node_id, Box::new(guard))),
        Ok(Err(e)) => bail!("niri screencast of {output}: {e}"),
        Err(_) => bail!("timed out starting the niri screencast of {output}"),
    }
}

fn cast_thread(
    setup_tx: Sender<Result<u32, String>>,
    stop: Arc<AtomicBool>,
    output: String,
    hw_cursor: bool,
) {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = setup_tx.send(Err(format!("build tokio runtime: {e}")));
            return;
        }
    };
    rt.block_on(async move {
        let session = match connect(&output, hw_cursor).await {
            Ok(s) => s,
            Err(e) => {
                let _ = setup_tx.send(Err(format!("{e:#}")));
                return;
            }
        };
        // A failed send means the opener gave up: stop now rather than hold a cast nobody reads.
        if setup_tx.send(Ok(session.node_id)).is_ok() {
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        if let Err(e) = session.session.call_method("Stop", &()).await {
            tracing::debug!(output = %output, error = %e, "niri screencast stop");
        }
    });
}

/// Held for the cast's lifetime; the connection is what niri ties the session to.
struct Session {
    session: zbus::Proxy<'static>,
    _conn: zbus::Connection,
    node_id: u32,
}

async fn connect(output: &str, hw_cursor: bool) -> Result<Session> {
    let conn = zbus::Connection::session()
        .await
        .context("connect session D-Bus")?;
    let sc = zbus::Proxy::new(
        &conn,
        BUS_SC,
        "/org/gnome/Mutter/ScreenCast",
        "org.gnome.Mutter.ScreenCast",
    )
    .await
    .context("niri ScreenCast proxy (is niri built with xdp-gnome-screencast?)")?;
    let props: HashMap<&str, Value> = HashMap::new();
    let path: OwnedObjectPath = sc
        .call("CreateSession", &(props,))
        .await
        .context("ScreenCast.CreateSession")?;
    let session =
        zbus::Proxy::new(&conn, BUS_SC, path, "org.gnome.Mutter.ScreenCast.Session").await?;

    let mut rec: HashMap<&str, Value> = HashMap::new();
    rec.insert(
        "cursor-mode",
        Value::from(if hw_cursor {
            CURSOR_METADATA
        } else {
            CURSOR_EMBEDDED
        }),
    );
    let stream_path: OwnedObjectPath = session
        .call("RecordMonitor", &(output, rec))
        .await
        .with_context(|| format!("Session.RecordMonitor({output:?})"))?;
    let stream = zbus::Proxy::new(
        &conn,
        BUS_SC,
        stream_path,
        "org.gnome.Mutter.ScreenCast.Stream",
    )
    .await?;
    // Subscribe before Start: the signal can land while the subscription is still being made.
    let mut added = stream
        .receive_signal("PipeWireStreamAdded")
        .await
        .context("subscribe PipeWireStreamAdded")?;
    session
        .call_method("Start", &())
        .await
        .context("ScreenCast.Session.Start")?;
    let msg = tokio::time::timeout(Duration::from_secs(10), added.next())
        .await
        .map_err(|_| anyhow!("PipeWireStreamAdded did not arrive within 10s"))?
        .ok_or_else(|| anyhow!("signal stream ended before PipeWireStreamAdded"))?;
    let (node_id,): (u32,) = msg
        .body()
        .deserialize()
        .context("PipeWireStreamAdded body")?;
    tracing::info!(output, node_id, "niri: screencast started");
    Ok(Session {
        session,
        _conn: conn,
        node_id,
    })
}
