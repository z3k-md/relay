use std::path::PathBuf;

use relay_core::{Device, DeviceId, LogicalPath, ObjectId, Sequence};
use relay_fs::ScanWarning;
use serde::Serialize;

/// Refuse a scan that would tombstone at least this many entries when they
/// also exceed half of the previous live set.
pub const MASS_DELETE_MIN_COUNT: usize = 25;

/// `deletions * MASS_DELETE_NUMERATOR > live * MASS_DELETE_DENOMINATOR`.
pub const MASS_DELETE_NUMERATOR: usize = 2;
pub const MASS_DELETE_DENOMINATOR: usize = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanOptions {
    pub allow_mass_delete: bool,
    pub dry_run: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub message: String,
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<&ScanWarning> for Warning {
    fn from(warning: &ScanWarning) -> Self {
        let message = match warning {
            ScanWarning::NonUtf8Name(path) => {
                format!("non-UTF-8 name at {}", path.display())
            }
            ScanWarning::InvalidName { os_path, reason } => {
                format!("invalid name at {}: {reason}", os_path.display())
            }
            ScanWarning::CaseCollision { a, b } => {
                format!("case collision between {a} and {b}")
            }
            ScanWarning::NotPortable { path, issue } => {
                format!("path {path} is not portable: {issue}")
            }
            ScanWarning::Unreadable { os_path, error } => {
                format!("unreadable {}: {error}", os_path.display())
            }
            ScanWarning::SpecialFile(path) => {
                format!("skipped special file {}", path.display())
            }
            ScanWarning::NestedMount(path) => {
                format!("skipped nested mount at {}", path.display())
            }
            ScanWarning::NormalizationCollision { path, os_paths } => {
                let first = os_paths
                    .first()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<unknown>".to_owned());
                let others = os_paths.len().saturating_sub(1);
                format!(
                    "{first} and {others} other on-disk names map to {path}; only the first is indexed"
                )
            }
        };
        Self { message }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ScanReport {
    pub created: usize,
    pub modified: usize,
    pub deleted: usize,
    pub stat_only: usize,
    pub unchanged: usize,
    pub unstable: Vec<LogicalPath>,
    pub protected: usize,
    pub deselected: Vec<LogicalPath>,
    pub warnings: Vec<Warning>,
    pub bytes_hashed: u64,
}

impl ScanReport {
    pub fn has_changes(&self) -> bool {
        self.created != 0
            || self.modified != 0
            || self.deleted != 0
            || self.stat_only != 0
            || !self.warnings.is_empty()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct VerifyReport {
    pub checked: usize,
    pub missing: Vec<ObjectId>,
    pub corrupt: Vec<ObjectId>,
}

#[derive(Clone, Debug, Serialize)]
pub struct GcReport {
    pub removed: usize,
    pub bytes_freed: u64,
    pub kept: usize,
    pub tmp_cleaned: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub device: Device,
    pub mounts: Vec<MountStatus>,
    pub object_count: u64,
    pub last_sequence: Sequence,
    pub peers: Vec<PeerStatus>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PeerStatus {
    pub name: String,
    pub id: DeviceId,
    pub addresses: Vec<String>,
    pub spaces: Vec<PeerSpaceStatus>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PeerSpaceStatus {
    pub space: String,
    pub received_seq: Sequence,
    pub acked_seq: Sequence,
    pub our_latest_seq: Sequence,
    pub last_sync_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MountStatus {
    pub space: String,
    pub mount: String,
    pub path: Option<PathBuf>,
    pub marker_ok: bool,
    pub marker_state: String,
    pub live_entries: usize,
    pub tombstones: usize,
    pub last_scan_ms: Option<i64>,
    pub last_error: Option<String>,
}

pub(crate) fn is_large_fraction_delete(deletions: usize, live: usize) -> bool {
    live > 0
        && deletions >= MASS_DELETE_MIN_COUNT
        && deletions * MASS_DELETE_NUMERATOR > live * MASS_DELETE_DENOMINATOR
}

pub(crate) fn is_mass_delete(deletions: usize, live: usize, scanned: usize) -> bool {
    if live == 0 {
        return false;
    }
    let empty_scan = scanned == 0 && deletions > 0;
    empty_scan || is_large_fraction_delete(deletions, live)
}
