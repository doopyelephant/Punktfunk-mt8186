//! The host's channel to the driver: the WUDFHost handle-duplication broker, and
//! [`IddTransport`], the remote source's edge into the driver.
//!
//! The AU and cursor sections are unnamed. The driver reaches them only through
//! handles this broker duplicates into its WUDFHost, delivered as bare values by
//! the owning IOCTL. The process handle doubles as the driver-death probe.
//! Evidence: `design/idd-push-security.md`.

use super::*;
use crate::windows::remote::section;
use crate::windows::remote::transport::{
    HandleTarget, Planted, RemoteEndpoint, Transport, TransportEvent,
};
use crate::{EncodeCtlSender, EncoderSite, SetEncodeSender};
use pf_driver_proto::encode::{SetEncodeReply, SetEncodeRequest};

/// A dead WUDFHost and an idle desktop both stop the source counter, so the death probe runs
/// only past this much staleness.
const STALE: Duration = Duration::from_secs(2);
/// At most one 0 ms wait per this.
const PROBE_EVERY: Duration = Duration::from_secs(1);

/// On IOCTL success the driver owns (and closes) the duplicates; on any failure the
/// deliverer reaps what it planted ([`Self::close_remote`]).
pub(super) struct ChannelBroker {
    /// WUDFHost process (`ProcessSharingDisabled` — exclusively pf-vdisplay's).
    /// `SYNCHRONIZE` doubles as the driver-death probe ([`Self::driver_alive`]).
    process: OwnedHandle,
    pub(super) wudf_pid: u32,
}

impl ChannelBroker {
    /// Open the WUDFHost duplication target. `wudf_pid == 0` is a driver that
    /// predates the sealed channel. [`open_wudfhost`] proves the image path before
    /// any section handle is duplicated into it: a spoofed ADD pid (same interface
    /// GUID, different process) would otherwise receive the stream.
    pub(super) fn open(wudf_pid: u32) -> Result<Self> {
        let process = open_wudfhost(wudf_pid, "driver-channel")?;
        Ok(Self { process, wudf_pid })
    }

    /// Raise WUDFHost's GPU scheduling class. The driver's encoder submits its GPU work
    /// there, so the host's own class covers none of it. Best-effort. `self.process` keeps
    /// the verified WUDFHost alive, so `wudf_pid` cannot name another process meanwhile.
    pub(super) fn raise_gpu_priority(&self) {
        // SAFETY: plain open by pid; the result is checked before use.
        let h = match unsafe { OpenProcess(PROCESS_SET_INFORMATION, false, self.wudf_pid) } {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(wudf_pid = self.wudf_pid, error = %e, "WUDFHost GPU priority not raised");
                return;
            }
        };
        // SAFETY: `h` was just opened here; `OwnedHandle` becomes its sole owner.
        let owned = unsafe { OwnedHandle::from_raw_handle(h.0 as _) };
        // SAFETY: `owned` is live for the call and carries PROCESS_SET_INFORMATION.
        unsafe {
            pf_frame::dxgi::elevate_gpu_priority_of(HANDLE(owned.as_raw_handle()), "WUDFHost")
        };
    }

    /// `SYNCHRONIZE` wait: signaled ⇔ WUDFHost exited. A dead driver and an idle desktop
    /// both just stop advancing the source counter, so this is the only death signal.
    pub(super) fn driver_alive(&self) -> bool {
        // SAFETY: `process` is this broker's live `OwnedHandle` (borrowed for the call); a
        // 0 ms wait only reads the handle's signaled state.
        unsafe { WaitForSingleObject(HANDLE(self.process.as_raw_handle()), 0) != WAIT_OBJECT_0 }
    }

    /// Duplicate `h` into the WUDFHost table. The returned value is valid only there.
    /// `Some(rights)` grants exactly those rights; `None` copies the source
    /// (`DUPLICATE_SAME_ACCESS`).
    pub(super) fn dup_into(&self, h: BorrowedHandle<'_>, access: Option<u32>) -> Result<u64> {
        let mut out = HANDLE::default();
        let (desired, options) = match access {
            Some(rights) => (rights, DUPLICATE_HANDLE_OPTIONS(0)),
            None => (0, DUPLICATE_SAME_ACCESS),
        };
        // SAFETY: `h` is borrowed, so live; `self.process` is the live PROCESS_DUP_HANDLE
        // target; `&mut out` is a valid out-param. Explicit mask (options == 0) or
        // `DUPLICATE_SAME_ACCESS` (desired ignored) — never both.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                HANDLE(h.as_raw_handle()),
                HANDLE(self.process.as_raw_handle()),
                &mut out,
                desired,
                false,
                options,
            )
        }
        .context("DuplicateHandle into the driver's WUDFHost")?;
        Ok(out.0 as usize as u64)
    }

    /// Duplicate a cursor section into WUDFHost with the same `SECTION_MAP_RW` as the
    /// AU section.
    pub(super) fn dup_into_public(&self, h: BorrowedHandle<'_>) -> Result<u64> {
        self.dup_into(h, Some(SECTION_MAP_RW))
    }

    /// Failure-path reaper for a cursor-channel duplicate the driver never adopted.
    pub(super) fn close_remote_public(&self, value: u64) {
        self.close_remote(value);
    }

    /// Close a handle VALUE in the WUDFHost table. `DUPLICATE_CLOSE_SOURCE` with no
    /// target closes the source; the result is ignored.
    fn close_remote(&self, value: u64) {
        if value == 0 {
            return;
        }
        // SAFETY: `self.process` is the live duplication target; `value` is a handle this
        // broker just created there and the driver never received. Closing it cannot touch
        // any other process's handles.
        unsafe {
            let _ = DuplicateHandle(
                HANDLE(self.process.as_raw_handle()),
                HANDLE(value as usize as *mut core::ffi::c_void),
                HANDLE::default(),
                std::ptr::null_mut(),
                0,
                false,
                DUPLICATE_CLOSE_SOURCE,
            );
        }
    }
}

impl HandleTarget for ChannelBroker {
    fn plant(&self, bytes: usize) -> Result<Planted> {
        section::plant(
            &|h, a| self.dup_into(h, a),
            &|v| self.close_remote(v),
            bytes,
        )
    }

    fn close_remote(&self, value: u64) {
        ChannelBroker::close_remote(self, value);
    }
}

/// The driver's side of the remote source: its WUDFHost, and the two IOCTLs the host sends.
pub(super) struct IddTransport {
    broker: Arc<ChannelBroker>,
    target_id: u32,
    /// The display's CCD key, as the log lines name it.
    name: String,
    set_encode: SetEncodeSender,
    encode_ctl: EncodeCtlSender,
    /// The last driver-death probe.
    probed: Instant,
}

impl IddTransport {
    pub(super) fn new(
        broker: Arc<ChannelBroker>,
        ccd: pf_win_display::win_display::CcdTargetKey,
        set_encode: SetEncodeSender,
        encode_ctl: EncodeCtlSender,
    ) -> Self {
        Self {
            broker,
            target_id: ccd.target_id,
            name: ccd.to_string(),
            set_encode,
            encode_ctl,
            probed: Instant::now(),
        }
    }
}

impl Transport for IddTransport {
    fn endpoint(&self) -> RemoteEndpoint {
        RemoteEndpoint {
            site: EncoderSite::Driver,
            target_id: self.target_id,
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn handles(&self) -> &dyn HandleTarget {
        &*self.broker
    }

    /// Every open re-raises WUDFHost's GPU scheduling class: the driver's encoder submits its
    /// work there, so the host's own class covers none of it.
    fn set_encode(&self, req: &SetEncodeRequest) -> Result<SetEncodeReply> {
        self.broker.raise_gpu_priority();
        (self.set_encode)(req)
    }

    fn encode_ctl(&self) -> EncodeCtlSender {
        self.encode_ctl.clone()
    }

    fn secure_desktop(&self) -> bool {
        pf_win_display::secure_desktop()
    }

    /// Probe while stale so the driver cycle fires instead of the session streaming nothing.
    fn poll(&mut self, now: Instant, last_fresh: Instant) -> Result<Vec<TransportEvent>> {
        if now.saturating_duration_since(last_fresh) <= STALE
            || now.saturating_duration_since(self.probed) <= PROBE_EVERY
        {
            return Ok(Vec::new());
        }
        self.probed = now;
        if self.broker.driver_alive() {
            return Ok(Vec::new());
        }
        tracing::warn!(
            wudf_pid = self.broker.wudf_pid,
            "IDD push: the pf-vdisplay WUDFHost is gone — firing the driver cycle"
        );
        Ok(vec![TransportEvent::DriverGone])
    }
}
