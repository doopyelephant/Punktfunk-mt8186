//! The Windows sources and the pure models they run on. A model compiles on every target,
//! so its tests run on Linux CI; a source is `cfg(windows)`.

#[cfg(target_os = "windows")]
pub mod gpu_pref;
#[cfg(target_os = "windows")]
#[path = "idd/mod.rs"]
pub(crate) mod idd_push;
// What the win32u GPU-preference patch writes, and the one head it may write over. Pure
// byte tables, so the test runs on every target.
mod win32u_stub;
// The remote-encoder source the driver and the capture worker share: its transport seam, the
// AU-section proxy, the supervisor. Pure but its Windows shells, so a whole session runs over
// a fake driver on every target.
pub(crate) mod remote;
// The recovery classifier's cursor-damage witness. Pure over `(now, kicked, position)` for the
// same reason: the rule that decides whether a silent desktop resets gets tests on every target.
mod cursor_witness;
// The IDD-push cursor render model: who draws the pointer and what the driver is told. Pure,
// so its table of past cursor defects runs on every target.
mod cursor_state;
// The IDD-push display depth pin: which depth the display holds, and when a change re-opens
// the driver's encoder. Pure over the colour seam, so its table runs on every target.
#[path = "idd/depth_pin.rs"]
mod depth_pin;
// The IDD-push open as a plan of steps, and the display's half run over the colour seam. Pure
// but for the seam, so a test per session kind runs on every target.
#[path = "idd/open_plan.rs"]
mod open_plan;
// The IDD-push tick: the wrapper's order around the remote source's phases. Pure, so its
// table of past tick defects runs on every target.
#[path = "idd/tick.rs"]
mod tick;
// The IDD-push stall model: holes in DWM delivery and their verdicts. Pure over `Instant`s,
// so its tests run on every target; `idd/stall.rs` writes the report.
mod stall_model;
// The GDI cursor poller's planes-to-RGBA rasterisers, pure over `&[u8]` for the same reason.
mod cursor_raster;
// A monitor the host did not create, captured and encoded by a capture worker.
#[cfg(target_os = "windows")]
pub mod synthetic_nv12;
#[cfg(target_os = "windows")]
pub(crate) mod wgc;
