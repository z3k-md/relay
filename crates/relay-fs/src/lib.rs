//! Filesystem scanning, mount markers, path mapping, and atomic writes.

pub mod cloud;
mod error;
mod marker;
mod materialize;
mod paths;
mod scan;
mod watch;

use std::path::Path;

pub use error::FsError;
pub use marker::MountMarker;
pub use materialize::{MaterializeOptions, materialize_file};
pub use paths::{
    check_real_dir_chain, component_is_normal, ensure_real_dir_chain, resolve_os_path,
    to_logical_path, to_os_path,
};
pub use scan::{
    PartialScan, ScanResult, ScanScope, ScanWarning, ScannedEntry, ScopeKind, effective_rules,
    scan_mount, scan_mount_with, scan_paths,
};
pub use watch::{MountWatcher, WatchSignal};

pub(crate) fn sync_parent_dir(dir: &Path) -> Result<(), FsError> {
    #[cfg(unix)]
    {
        let file = std::fs::File::open(dir).map_err(|e| FsError::io(dir, e))?;
        file.sync_all().map_err(|e| FsError::io(dir, e))?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}
