//! Filesystem scanning, mount markers, path mapping, and atomic writes.

mod error;
mod marker;
mod materialize;
mod paths;
mod scan;

use std::path::Path;

pub use error::FsError;
pub use marker::MountMarker;
pub use materialize::materialize_file;
pub use paths::{to_logical_path, to_os_path};
pub use scan::{ScanResult, ScanWarning, ScannedEntry, effective_rules, scan_mount};

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
