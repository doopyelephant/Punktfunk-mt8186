//! Display-topology seam: the calls the isolate ladder makes into `pf-win-display`.
//!
//! [`CcdOps`] is how [`super::isolate_txn`], the exclusive watch's survivor read and its
//! re-assert reach the OS. [`WinCcd`] is the one production implementor and passes each
//! call straight on. The trait remains so tests can script what an isolate observed.
//!
//! The manager's other `pf_win_display` calls are direct.

use std::sync::Arc;

use pf_win_display::snapshot::DisplaySnapshot;
use pf_win_display::win_display::{CcdTargetKey, IsolateOutcome, SavedConfig};

/// `Send + Sync` because the manager is a `&'static` singleton the watch thread reads.
pub(crate) trait CcdOps: Send + Sync {
    /// [`pf_win_display::win_display::isolate_displays_ccd_checked`].
    fn isolate_displays_ccd_checked(
        &self,
        keep: &[CcdTargetKey],
        store: bool,
    ) -> Option<(SavedConfig, IsolateOutcome)>;
    /// [`pf_win_display::display_events::snapshot_or_query`].
    fn snapshot_or_query(&self) -> Arc<DisplaySnapshot>;
    /// [`pf_win_display::monitor_devnode::disable_connected_inactive`].
    fn disable_connected_inactive(
        &self,
        keep: &[CcdTargetKey],
        baseline_active: &[CcdTargetKey],
        generation: u64,
    ) -> Vec<String>;
}

/// The OS, through `pf-win-display`.
pub(crate) struct WinCcd;

impl CcdOps for WinCcd {
    fn isolate_displays_ccd_checked(
        &self,
        keep: &[CcdTargetKey],
        store: bool,
    ) -> Option<(SavedConfig, IsolateOutcome)> {
        pf_win_display::win_display::isolate_displays_ccd_checked(keep, store)
    }

    fn snapshot_or_query(&self) -> Arc<DisplaySnapshot> {
        pf_win_display::display_events::snapshot_or_query()
    }

    fn disable_connected_inactive(
        &self,
        keep: &[CcdTargetKey],
        baseline_active: &[CcdTargetKey],
        generation: u64,
    ) -> Vec<String> {
        pf_win_display::monitor_devnode::disable_connected_inactive(
            keep,
            baseline_active,
            generation,
        )
    }
}
