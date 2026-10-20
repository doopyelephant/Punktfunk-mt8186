//! The remote-encoder source: a capture source whose encoder runs in another process, the
//! pf-vdisplay driver's WUDFHost (IDD-push) or a capture worker (WGC). One
//! [`source::RemoteSource`] serves both over a [`transport::Transport`]; the encoder proxy the
//! loop drives reads the AU section both publish into.
//!
//! Every module but the Windows shells (`broker`, `section`, `qpc`, `clock_boost`) is pure, so
//! the whole session runs over a fake driver on every target (`fake`). Evidence:
//! `design/pf-capture-overhaul.md` §4.4, `design/windows-video-plane-overhaul.md` §2.4.

// Off Windows only the tests read these modules.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

pub(crate) mod au_assembly;
pub(crate) mod au_reader;
pub(crate) mod delivery;
pub(crate) mod encoder;
pub(crate) mod link;
pub(crate) mod source;
pub(crate) mod supervisor;
pub(crate) mod transport;

#[cfg(target_os = "windows")]
pub(crate) mod broker;
#[cfg(target_os = "windows")]
pub(crate) mod clock_boost;
#[cfg(target_os = "windows")]
pub(crate) mod qpc;
#[cfg(target_os = "windows")]
pub(crate) mod section;
