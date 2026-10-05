//! Windows shell integration for Relay Explorer.
//!
//! This is the only crate in the workspace that may use `unsafe`, and only
//! inside the `win` module. Everything it exports is a safe API. On other
//! platforms the crate is empty; callers gate their use on `cfg(windows)`.
//!
//! Shell COM objects need a single-threaded apartment. Work that may block
//! (images) runs on [`sta::StaPool`]; work that owns UI state (context menus,
//! drag and drop) must run on the window's own thread, which Tauri's main
//! thread already initialises for OLE.

#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use win::{Error, dnd, enumerate, fileop, image, memory, menu, sta, watch};
