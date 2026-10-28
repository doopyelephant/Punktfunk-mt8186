//! The KWin output seam: where each output operation picks its half.
//!
//! Topology, position, custom mode, re-enable and list each run in-process over
//! `kde_output_management_v2` ([`super::mgmt`]) first. When that half declines, the reason is
//! one log line and `kscreen-doctor` ([`super::kscreen`]) runs once; [`Resolved`] names the half
//! that answered. [`gate`] is the in-process half's answer before it addresses an output: a
//! connection that is bound, caught up, and new enough for the request.
//!
//! Pinned by the table in this file's tests, over (management bound, version, barrier).

use super::kscreen;
use super::mgmt::{self, TopologyKind};
use std::fmt;

/// `kde_output_configuration_v2.set_custom_modes` is `since 18`.
pub(crate) const CUSTOM_MODES_SINCE: u32 = 18;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Topology,
    Position,
    CustomMode,
    Reenable,
    List,
}

/// Why the in-process half declined an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Declined {
    /// No Wayland connection.
    NoWayland(String),
    /// Connected, but no management connection to use.
    Unbound(&'static str),
    /// A roundtrip missed its budget: the compositor is not answering this client.
    Barrier(&'static str),
    /// The bound management version lacks the request.
    Version { bound: u32, needs: u32 },
    /// What the operation addresses is not on the connection.
    Unresolved(String),
    /// KWin did not confirm the apply in budget; its failure reason when it sent one.
    Unconfirmed(Option<String>),
}

impl fmt::Display for Declined {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Declined::NoWayland(e) => write!(f, "no Wayland connection ({e})"),
            Declined::Unbound(what) => f.write_str(what),
            Declined::Barrier(what) => {
                write!(f, "the compositor did not answer the {what} in budget")
            }
            Declined::Version { bound, needs } => {
                write!(
                    f,
                    "management is bound at v{bound}, the request needs v{needs}"
                )
            }
            Declined::Unresolved(what) => write!(f, "{what} did not resolve on this connection"),
            Declined::Unconfirmed(None) => f.write_str("KWin did not confirm the apply"),
            Declined::Unconfirmed(Some(reason)) => {
                write!(f, "KWin did not confirm the apply ({reason})")
            }
        }
    }
}

/// The half that answered an operation.
#[derive(Debug, PartialEq)]
pub(crate) enum Resolved<I, K = I> {
    InProcess(I),
    Kscreen(K),
    /// The in-process half declined, and `kscreen-doctor` did not run or did not answer.
    Neither(Declined),
}

/// The in-process half's answer before it addresses an output. `seen` is the connection's
/// bound management version and whether it caught up in budget; `None` is no connection.
pub(crate) fn gate(op: Op, seen: Option<(u32, bool)>) -> Result<(), Declined> {
    let Some((bound, caught_up)) = seen else {
        return Err(Declined::Unbound("no output-management connection"));
    };
    if !caught_up {
        return Err(Declined::Barrier("catch-up roundtrip"));
    }
    match op {
        Op::CustomMode if bound < CUSTOM_MODES_SINCE => Err(Declined::Version {
            bound,
            needs: CUSTOM_MODES_SINCE,
        }),
        _ => Ok(()),
    }
}

/// Whether `kscreen-doctor` runs after the in-process half declined `op`. Not for a custom
/// mode on a bind below v18: `addCustomMode` needs the same request, so its 6–9 spawns
/// only delay the create's recreate at 60 Hz.
fn falls_back(op: Op, why: &Declined) -> bool {
    !matches!((op, why), (Op::CustomMode, Declined::Version { .. }))
}

/// `in_process`'s answer, else `kscreen` once when [`falls_back`] allows it. The decline is
/// logged here, once: no Wayland at debug (the console lists monitors outside any session).
pub(crate) fn resolve<I, K>(
    op: Op,
    in_process: Result<I, Declined>,
    kscreen: impl FnOnce() -> Option<K>,
) -> Resolved<I, K> {
    let why = match in_process {
        Ok(r) => return Resolved::InProcess(r),
        Err(why) => why,
    };
    let fallback = falls_back(op, &why);
    match &why {
        Declined::NoWayland(_) => {
            tracing::debug!(?op, reason = %why, fallback, "KWin in-process output management declined")
        }
        Declined::Version { .. } => {
            tracing::info!(?op, reason = %why, fallback, "KWin in-process output management declined")
        }
        _ => {
            tracing::warn!(?op, reason = %why, fallback, "KWin in-process output management declined")
        }
    }
    match fallback.then(kscreen).flatten() {
        Some(r) => Resolved::Kscreen(r),
        None => Resolved::Neither(why),
    }
}

/// Place the just-created output `our_prefix` (at `dims`) for `kind` on `sess` (the create's
/// [`mgmt::watch`]), `journal` getting the heads `Exclusive` darkens before the disable.
/// In-process answers our output's UUID; `kscreen-doctor` the kscreen address it resolved,
/// for every kind, so a later position never addresses the shared name.
pub(crate) fn topology(
    mut sess: Option<&mut mgmt::Session>,
    name: &str,
    our_prefix: &str,
    dims: (u32, u32),
    kind: TopologyKind,
    mut journal: impl FnMut(&[(String, String)]),
) -> Resolved<Option<String>, String> {
    let (w, h) = dims;
    let in_process =
        mgmt::apply_topology(sess.as_deref_mut(), our_prefix, w, h, kind, &mut journal);
    resolve(Op::Topology, in_process, || {
        let addr = kscreen::resolve_kscreen_addr(name, w, h);
        match kind {
            TopologyKind::Exclusive => kscreen::apply_virtual_primary(&addr, &mut journal),
            TopologyKind::Primary => kscreen::apply_virtual_primary_only(&addr),
            // Extend issues no kscreen apply; a stored mirror source is cleared in-process.
            TopologyKind::Extend => mgmt::clear_replication_source(sess, our_prefix, w, h),
        }
        Some(addr)
    })
}

/// Place our output `ours` at `(x, y)`: in-process on the create's kept `watch`, else at the
/// kscreen address the create resolved.
pub(crate) fn position(
    watch: Option<&mut mgmt::Session>,
    ours: &str,
    uuid: Option<&str>,
    kscreen_addr: Option<&str>,
    x: i32,
    y: i32,
) -> Resolved<()> {
    let in_process = mgmt::set_position(watch, ours, uuid, x, y);
    resolve(Op::Position, in_process, || {
        kscreen::set_position(kscreen_addr?, x, y).then_some(())
    })
}

/// `(width, height, refresh_hz)` of an output's mode.
pub(crate) type ModeHz = (u32, u32, u32);

/// Install and select `want` on our output, which sits at `at` now. Both halves answer the
/// active mode they read back; `kscreen-doctor` also the address it used.
pub(crate) fn custom_mode(
    sess: Option<&mut mgmt::Session>,
    name: &str,
    our_prefix: &str,
    at: (u32, u32),
    want: ModeHz,
) -> Resolved<ModeHz, (String, Option<ModeHz>)> {
    let (w, h, hz) = want;
    let in_process = mgmt::set_custom_mode(sess, our_prefix, at.0, at.1, w, h, hz);
    resolve(Op::CustomMode, in_process, || {
        let addr = kscreen::resolve_kscreen_addr(name, at.0, at.1);
        let active = kscreen::set_custom_refresh(w, h, hz, &addr);
        Some((addr, active))
    })
}

/// Re-enable `heads` (`(name, "WxH@Hz")`) at their modes.
pub(crate) fn reenable(heads: &[(String, String)]) -> Resolved<()> {
    resolve(Op::Reenable, mgmt::reenable_outputs(heads), || {
        kscreen::reenable_outputs_kscreen(heads);
        Some(())
    })
}

/// Every head KWin reports. A miss of both halves is an error, not "no monitors":
/// `monitors::resolve` treats it as one, so a pinned capture monitor refuses to start.
pub(crate) fn list() -> anyhow::Result<Vec<crate::monitors::PhysicalMonitor>> {
    match resolve(Op::List, mgmt::list_monitors(), kscreen::list_monitors) {
        Resolved::InProcess(heads) | Resolved::Kscreen(heads) => Ok(heads),
        Resolved::Neither(why) => Err(anyhow::anyhow!(
            "kscreen-doctor -j did not answer either (not installed, or killed at its budget): \
             KWin output management declined: {why}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Half {
        InProcess,
        Kscreen,
        Neither,
    }

    fn half<I, K>(r: &Resolved<I, K>) -> Half {
        match r {
            Resolved::InProcess(_) => Half::InProcess,
            Resolved::Kscreen(_) => Half::Kscreen,
            Resolved::Neither(_) => Half::Neither,
        }
    }

    /// Each operation runs in-process on a connection that is bound, caught up and new enough
    /// for it, and otherwise runs `kscreen-doctor` exactly once, except a custom mode below
    /// v18, which `kscreen-doctor` cannot install either.
    #[test]
    fn each_operation_runs_in_process_then_kscreen_once() {
        use Half::*;
        use Op::*;
        // (operation, (bound version, caught up), kscreen answers) → (half, kscreen runs)
        let rows = [
            (Topology, Some((22, true)), true, (InProcess, false)),
            (Topology, None, true, (Kscreen, true)),
            (Topology, Some((22, false)), true, (Kscreen, true)),
            (Topology, None, false, (Neither, true)),
            (Position, Some((22, true)), false, (InProcess, false)),
            (Position, None, true, (Kscreen, true)),
            (CustomMode, Some((18, true)), true, (InProcess, false)),
            (CustomMode, Some((17, true)), true, (Neither, false)),
            (CustomMode, Some((17, true)), false, (Neither, false)),
            (CustomMode, Some((22, false)), true, (Kscreen, true)),
            (Reenable, Some((13, true)), true, (InProcess, false)),
            (Reenable, Some((13, false)), true, (Kscreen, true)),
            (List, Some((3, true)), false, (InProcess, false)),
            (List, None, true, (Kscreen, true)),
            (List, None, false, (Neither, true)),
        ];
        for (op, seen, answers, (want, runs)) in rows {
            let mut ran = 0;
            let got = resolve(op, gate(op, seen), || {
                ran += 1;
                answers.then_some(())
            });
            assert_eq!(half(&got), want, "{op:?} {seen:?}");
            assert_eq!(ran, usize::from(runs), "{op:?} {seen:?}: kscreen runs");
        }
    }

    /// A decline past the gate (our output missing, an unconfirmed apply) falls back the same.
    #[test]
    fn a_decline_after_the_gate_falls_back_once() {
        for why in [
            Declined::Unresolved("our virtual output".into()),
            Declined::Unconfirmed(Some("mode not found".into())),
        ] {
            let got = resolve(Op::Topology, Err::<(), _>(why.clone()), || Some("7"));
            assert_eq!(got, Resolved::Kscreen("7"), "{why}");
            let got = resolve(Op::Topology, Err::<(), _>(why.clone()), || None::<()>);
            assert_eq!(got, Resolved::Neither(why));
        }
    }

    /// The version gate is the protocol's: v18 carries `set_custom_modes`, v17 does not.
    #[test]
    fn only_custom_modes_need_v18() {
        assert_eq!(
            gate(Op::CustomMode, Some((17, true))),
            Err(Declined::Version {
                bound: 17,
                needs: 18
            })
        );
        for op in [Op::Topology, Op::Position, Op::Reenable, Op::List] {
            assert_eq!(gate(op, Some((1, true))), Ok(()), "{op:?}");
        }
    }
}
