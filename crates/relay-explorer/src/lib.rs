//! Platform-neutral model for Relay Explorer.
//!
//! Listings stream in timed batches so the first rows paint before a large
//! folder finishes enumerating; watcher events are coalesced and resolved to
//! entries before they reach the UI. Windows-specific calls live in
//! `relay-shell-win`; other platforms use `std::fs` and `notify`.

pub mod bench;
pub mod effect;
pub mod entry;
pub mod image_url;
pub mod list;
pub mod watch;

pub use entry::Entry;
