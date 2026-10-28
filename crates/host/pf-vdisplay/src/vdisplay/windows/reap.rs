//! The ghost-monitor reap: removes the not-present "punktfunk" monitor devnodes that
//! `IddCxMonitorDeparture` leaves behind, so they stop pinning IddCx monitor slots.
//!
//! [`reap_ghost_monitors`] is best-effort and logs what it found and removed. The driver's
//! control-device open and the exhausted-ADD retry call it.

// Presence, not health: `Status -ne 'OK'` would `pnputil /remove-device` a live monitor in a
// transient problem state. `Status -eq 'Unknown'` guards `Present` reading null (`-not $null`
// is true and would select every device). Full-path pnputil; `$LASTEXITCODE=1` before launch
// so a miss cannot look like exit 0. Tokens are locale-invariant.
const REAP_PS: &str = "$ErrorActionPreference='SilentlyContinue'; \
    $g = @(Get-PnpDevice -Class Monitor | Where-Object { -not $_.Present -and $_.Status -eq 'Unknown' -and $_.FriendlyName -match 'punktfunk' }); \
    $pnp = ($env:SystemRoot + '\\System32\\pnputil.exe'); \
    $n = 0; foreach ($d in $g) { $LASTEXITCODE = 1; if (Test-Path $pnp) { & $pnp /remove-device $d.InstanceId *> $null }; if ($LASTEXITCODE -eq 0) { $n++ } }; \
    Write-Output ($g.Count.ToString() + ' ' + $n)";

/// Remove not-present "punktfunk" monitor PDOs that `IddCxMonitorDeparture` leaves behind.
/// Each ghost pins a VidPN target against IddCx's ~16-slot budget; once full, `IOCTL_ADD`
/// returns 0x80070490 (`ERROR_NOT_FOUND`). Best-effort: only `Present==false` AND
/// `Status==Unknown` nodes are removed, so a live session is never touched. Returns how many
/// were removed. Logs found and removed even when both are zero — silence hid a failed reap.
pub(super) fn reap_ghost_monitors() -> u32 {
    // Full-path powershell: LocalSystem PATH need not include System32.
    let ps = pf_paths::system32(pf_paths::POWERSHELL);
    // Bounded: this runs under the manager's `device` mutex (driver open) and under its `state`
    // lock (the ADD slot-exhaustion retry), so a wedged Get-PnpDevice would block every acquire,
    // release and `/display/state`. `output_within` kills the whole tree on the deadline.
    let mut cmd = std::process::Command::new(&ps);
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        REAP_PS,
    ]);
    match crate::proc::output_within(&mut cmd, std::time::Duration::from_secs(30)) {
        Ok(o) => {
            let raw = String::from_utf8_lossy(&o.stdout);
            let Some((found, removed)) = parse_reap_output(&raw) else {
                tracing::warn!(
                    output = %raw.trim(),
                    "pf-vdisplay: ghost-monitor reap died before reporting — ghost nodes (if any) still pin IddCx monitor slots"
                );
                return 0;
            };
            if found == 0 {
                tracing::info!("pf-vdisplay: no ghost (not-present) virtual-monitor nodes to reap");
            } else if removed < found {
                tracing::warn!(
                    found,
                    removed,
                    "pf-vdisplay: ghost-monitor reap left ghost nodes behind — the leftovers keep pinning IddCx monitor slots toward the 0x80070490 wedge"
                );
            } else {
                tracing::warn!(
                    reaped = removed,
                    "pf-vdisplay: reaped ghost (not-present) virtual-monitor nodes — IddCx slot-exhaustion prevention"
                );
            }
            removed
        }
        Err(e) => {
            tracing::warn!(error = %e, "pf-vdisplay: ghost-monitor reap could not spawn powershell");
            0
        }
    }
}

/// Parse `"<found> <removed>"` from [`reap_ghost_monitors`]. `None` = the script died before
/// reporting (callers treat that as removed-nothing, loudly).
fn parse_reap_output(out: &str) -> Option<(u32, u32)> {
    let mut it = out.split_whitespace().map(str::parse::<u32>);
    match (it.next(), it.next()) {
        (Some(Ok(found)), Some(Ok(removed))) => Some((found, removed)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Found and removed travel separately so a leftover ghost is loud. A single number or
    /// a script that died before reporting must not decode as a reap.
    #[test]
    fn reap_output_decodes_found_and_removed() {
        assert_eq!(parse_reap_output("3 3\r\n"), Some((3, 3)));
        assert_eq!(
            parse_reap_output("4 0"),
            Some((4, 0)),
            "pnputil unlaunchable"
        );
        assert_eq!(parse_reap_output("0 0"), Some((0, 0)), "clean box");
        for dead in ["5", "", "   ", "garbage", "OK"] {
            assert_eq!(
                parse_reap_output(dead),
                None,
                "{dead:?} is not a reap report"
            );
        }
    }
}
