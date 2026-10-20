//! The Windows sources and the pure models they run on. A model compiles on every target,
//! so its tests run on Linux CI; a source is `cfg(windows)`.

#[cfg(target_os = "windows")]
pub mod dxgi;
#[cfg(target_os = "windows")]
pub(crate) mod idd_push;
// What the win32u GPU-preference patch writes, and the one head it may write over. Pure
// byte tables, so the test runs on every target.
mod win32u_stub;
// The AU section's reader half. Pure over a mapped view, so its tests run on every target.
mod au_reader;
// The driver-encode proxy's chunk and AU assembly over that reader. Pure for the same reason.
mod au_assembly;
// The recovery classifier's cursor-damage witness. Pure over `(now, kicked, position)` for the
// same reason: the rule that decides whether a silent desktop resets gets tests on every target.
mod cursor_witness;
// The IDD-push cursor render model: who draws the pointer and what the driver is told. Pure,
// so its table of past cursor defects runs on every target.
mod cursor_state;
// The delivery gate and the frame shape of the two capturers that hand over no pixels. Pure.
mod pixelless;
// The IDD-push stall model: holes in DWM delivery and their verdicts. Pure over `Instant`s,
// so its tests run on every target; `idd_push/stall.rs` writes the report.
mod stall_model;
// The GDI cursor poller's planes-to-RGBA rasterisers, pure over `&[u8]` for the same reason.
mod cursor_raster;
// The host's end of a capture worker's pipes. Generic over the pipes and the process, so its
// tests stand in for a worker on every target.
mod worker_link;
// A monitor the host did not create, captured and encoded by a capture worker.
#[cfg(target_os = "windows")]
pub mod synthetic_nv12;
#[cfg(target_os = "windows")]
pub(crate) mod wgc;
