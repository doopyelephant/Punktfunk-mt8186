//! Windows capture GPU mechanics: the win32u GPU-preference hook and the P010
//! self-test. The D3D11 converters (`HdrP010Converter`, `VideoConverter`, …)
//! live in `pf_encode_win::convert`; [`super::idd_push`] imports them there.
//!
//! Capture identity ([`WinCaptureTarget`], [`D3d11Frame`], [`pack_luid`],
//! [`make_device`]) lives in `pf-frame` so capture, encode, and pf-vdisplay share
//! one type without a crate cycle. This module re-exports them so `crate::dxgi::*`
//! still resolves. There is no capturer here.

pub use pf_frame::dxgi::{make_device, pack_luid, D3d11Frame, PyroFrameShare, WinCaptureTarget};

// `#[path]`: this file is itself reached through a `#[path]` from `lib.rs`, so a
// bare `mod selftest;` would resolve to `windows/selftest.rs`.
#[path = "dxgi/selftest.rs"]
mod selftest;
pub use selftest::{hdr_p010_convert_bars_on_luid, hdr_p010_selftest_at};

#[cfg(target_arch = "x86_64")]
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

/// Hits on the hooked `NtGdiDdDDIGetCachedHybridQueryValue`. Patch-readback in
/// [`install_gpu_pref_hook`] only proves the bytes landed; `0` here means DXGI
/// never reached the export on this build (always on ARM64, where no patch exists).
static HYBRID_HOOK_HITS: AtomicU64 = AtomicU64::new(0);

pub fn hybrid_hook_hits() -> u64 {
    HYBRID_HOOK_HITS.load(Ordering::Relaxed)
}

// Declared here so we skip the Win32_System_Diagnostics_Debug feature for one call.
// DXGI runs the hooked export on the encode worker, possibly another core;
// FlushInstructionCache after the patch so that core does not keep the old bytes.
#[cfg(target_arch = "x86_64")]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn FlushInstructionCache(h: *mut c_void, base: *const c_void, size: usize) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
}
/// Always report `D3DKMT_GPU_PREFERENCE_STATE_UNSPECIFIED` (3). Replaces the
/// export in full, so there is no trampoline back to the original.
#[cfg(target_arch = "x86_64")]
unsafe extern "system" fn hybrid_query_hook(gpu_preference: *mut u32) -> i32 {
    HYBRID_HOOK_HITS.fetch_add(1, Ordering::Relaxed);
    if gpu_preference.is_null() {
        return 0xC000_000Du32 as i32; // STATUS_INVALID_PARAMETER
    }
    // SAFETY: win32u's contract for this export — the caller (DXGI) passes a writable `*mut u32`
    // out-param — and the null case has just been rejected above, so this is an in-bounds,
    // 4-aligned single-word store into the caller's live local.
    unsafe { *gpu_preference = 3 }; // D3DKMT_GPU_PREFERENCE_STATE_UNSPECIFIED
    0 // STATUS_SUCCESS
}

/// Fake `D3DKMT_GPU_PREFERENCE_STATE_UNSPECIFIED` so DXGI skips hybrid
/// GPU-preference resolution. Without this, DXGI reparents outputs onto the
/// preferred render GPU and ignores `SET_RENDER_ADAPTER`, so the driver's
/// swap-chain lands on an adapter its encoder cannot open.
///
/// Call once from `main.rs` before the first DXGI factory. Lasts the process
/// lifetime. [`hybrid_hook_hits`] reports whether DXGI actually calls it.
/// Also sets per-monitor DPI awareness; on ARM64 that half is all that runs.
///
/// # Invariant
///
/// The patch rewrites 12 live bytes of `win32u.dll`, and only over the syscall stub
/// ([`is_syscall_stub`]): any other head is refused and the export left as found. Nothing
/// here checks the other half. No other thread is inside the export while its bytes change:
/// only DXGI calls it, so this must run before any thread creates a DXGI factory. The call
/// order in the host's `startup` is the only enforcement.
pub fn install_gpu_pref_hook() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        set_dpi_awareness();
        // The patch is x86-64 machine code; a Snapdragon SoC has one GPU, so DXGI has
        // nothing to reparent there and the DPI half above is all ARM64 needs.
        #[cfg(not(target_arch = "x86_64"))]
        tracing::info!("GPU-pref hook: x86-64 only, skipped on this architecture");
        #[cfg(target_arch = "x86_64")]
        patch_hybrid_query();
    });
}

/// Make the process per-monitor DPI aware unless it already is. Below that the OS virtualizes
/// window and cursor coordinates while host geometry is CCD physical pixels, so `SetCursorPos`
/// and the cursor blend miss on a scaled display. The host exe declares v2 in its manifest
/// (punktfunk-host/build.rs); this covers every other process using DXGI.
fn set_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        DPI_AWARENESS_PER_MONITOR_AWARE,
    };
    // 0=UNAWARE 1=SYSTEM 2=PER_MONITOR(_V2).
    // SAFETY: both calls take and return a context handle by value and reach no memory of ours.
    let awareness =
        || unsafe { GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()).0 };
    if awareness() < DPI_AWARENESS_PER_MONITOR_AWARE.0 {
        // SAFETY: a constant context handle by value; the call changes this process's DPI mode
        // and nothing else.
        match unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
            Ok(()) => tracing::info!("DPI awareness set: PER_MONITOR_AWARE_V2"),
            Err(e) => tracing::warn!(error = ?e,
                "SetProcessDpiAwarenessContext failed — cursor/desktop coordinates may be \
                 DPI-virtualized against the host's physical-pixel CCD geometry"),
        }
    }
    tracing::info!(
        awareness = awareness(),
        "effective DPI awareness (need 2=PER_MONITOR for physical-pixel coordinates)"
    );
}

/// Overwrite the head of `win32u!NtGdiDdDDIGetCachedHybridQueryValue` with a jump to
/// [`hybrid_query_hook`], and say in the log whether the bytes landed. A head that is not the
/// syscall stub is not written to.
#[cfg(target_arch = "x86_64")]
fn patch_hybrid_query() {
    use crate::win32u_stub::{is_syscall_stub, jump_to, PATCH_LEN};
    use windows::core::s;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
    use windows::Win32::System::Memory::{
        VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
    };
    // SAFETY: the name is a NUL-terminated literal. The module handle is never freed, so
    // win32u stays mapped for the life of the process.
    let Ok(lib) = (unsafe { LoadLibraryA(s!("win32u.dll")) }) else {
        tracing::warn!(
            "GPU-pref hook: win32u.dll not loadable — skipping (on a hybrid-GPU box DXGI may \
             reparent the virtual display off the pinned render adapter → TEX_FAIL rebinds)"
        );
        return;
    };
    // SAFETY: `lib` is the live module handle from above and the name a NUL-terminated literal.
    let Some(target) = (unsafe { GetProcAddress(lib, s!("NtGdiDdDDIGetCachedHybridQueryValue")) })
    else {
        tracing::warn!(
            "GPU-pref hook: NtGdiDdDDIGetCachedHybridQueryValue not exported — skipping"
        );
        return;
    };
    let stub = target as usize as *mut [u8; PATCH_LEN];
    let patch = jump_to(hybrid_query_hook as *const () as usize);
    let mut old = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: `old` is a live out-param. The call changes page protection only, and fails on
    // a range that is not committed memory of this process instead of touching it.
    let writable = unsafe {
        VirtualProtect(
            stub as *const c_void,
            PATCH_LEN,
            PAGE_EXECUTE_READWRITE,
            &mut old,
        )
    };
    if writable.is_err() {
        tracing::warn!("GPU-pref hook: VirtualProtect failed — skipping");
        return;
    }
    // SAFETY: `VirtualProtect` took these 12 bytes, so they are committed, and readable and
    // writable until the call below hands their protection back. Both reads run before it.
    // A byte array has no alignment and no invalid value.
    let read = || unsafe { stub.read() };
    let found = read();
    // A head that is not the stub is left alone: its instructions may end mid-patch, or
    // another module's jump may live there.
    let readback = is_syscall_stub(&found).then(|| {
        // SAFETY: writable as above. `found` is the syscall stub: it is entered at its first
        // byte only and its one branch lands past these 12, so every entry runs the whole
        // jump. No thread executes them now: only DXGI calls the export, and this runs before
        // any DXGI factory exists (the invariant on `install_gpu_pref_hook`).
        unsafe { stub.write(patch) };
        // A write the OS rejected silently reads back as the stub.
        read()
    });
    let mut restore = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: the same 12 bytes, back to the protection they had; `restore` is a live out-param.
    let _ = unsafe { VirtualProtect(stub as *const c_void, PATCH_LEN, old, &mut restore) };
    let Some(readback) = readback else {
        tracing::warn!(
            found = %format!("{found:02x?}"),
            "GPU-pref hook: the export does not start with the win32u syscall stub — skipping \
             (on a hybrid-GPU box DXGI may reparent the virtual display off the pinned render \
             adapter → TEX_FAIL rebinds)"
        );
        return;
    };
    // Patch is on the main thread; DXGI calls the export from the encode worker,
    // possibly another core with a stale i-cache that would still run the original.
    // SAFETY: this process's pseudo-handle, and the 12 bytes just written, still mapped.
    let _ = unsafe { FlushInstructionCache(GetCurrentProcess(), stub as *const c_void, PATCH_LEN) };
    if readback == patch {
        tracing::info!(
            "GPU-pref hook installed + verified (win32u hybrid-query -> UNSPECIFIED): DXGI \
             output reparenting disabled. Whether DXGI actually CALLS it shows up as \
             hybrid_hook_hits on the IDD-push open line."
        );
    } else {
        tracing::error!(
            want = %format!("{patch:02x?}"), got = %format!("{readback:02x?}"),
            "GPU-pref hook patch did NOT land — hook is DEAD (on a hybrid-GPU box DXGI can \
             still reparent the virtual display off the pinned render adapter)"
        );
    }
}
