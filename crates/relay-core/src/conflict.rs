//! Deterministic conflict resolution.
//!
//! Every device must reach the same outcome from the same two versions, or the
//! "resolved" entry stays divergent. Nothing here may depend on which device is
//! doing the computing or on local wall-clock time.

use crate::entry::EntryRecord;
use crate::error::CoreError;
use crate::ids::DeviceId;
use crate::path::LogicalPath;

/// Marker inserted into conflict-copy names. The original extension is
/// deliberately pushed out of final position so build tools and test runners
/// do not pick up conflict copies (`foo.go` -> `foo.go.relay-conflict-...`).
pub const CONFLICT_MARKER: &str = ".relay-conflict-";

/// Which of two concurrent versions keeps the original path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictWinner {
    A,
    B,
}

/// The version whose writing device holds the higher counter wins, with the
/// device id as a tiebreak. Both inputs are symmetric, so any device computes
/// the same answer.
pub fn choose_winner(a: &EntryRecord, b: &EntryRecord) -> ConflictWinner {
    let rank = |r: &EntryRecord| (r.vector.get(&r.modified_by), r.modified_by);
    if rank(a) >= rank(b) {
        ConflictWinner::A
    } else {
        ConflictWinner::B
    }
}

/// Path for the losing version, derived only from replicated data.
pub fn conflict_path(
    original: &LogicalPath,
    losing_device: &DeviceId,
    losing_counter: u64,
) -> Result<LogicalPath, CoreError> {
    let name = format!(
        "{}{}{}-{}",
        original.file_name(),
        CONFLICT_MARKER,
        losing_device.short(),
        losing_counter
    );
    match original.parent() {
        Some(parent) => parent.join(&name),
        None => LogicalPath::new(&name),
    }
}

pub fn is_conflict_copy(path: &LogicalPath) -> bool {
    path.file_name().contains(CONFLICT_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{EntryContent, EntryKey};
    use crate::ids::{MountId, ObjectId, Sequence, SpaceId};

    fn record(device: u8, data: &[u8], now_ms: i64) -> EntryRecord {
        EntryRecord::local_write(
            None,
            EntryKey {
                space: SpaceId::new(),
                mount: MountId::new(),
                path: LogicalPath::new("src/foo.go").unwrap(),
            },
            EntryContent::File {
                object: ObjectId::of(data),
                size: data.len() as u64,
                executable: false,
            },
            None,
            DeviceId::from_bytes([device; 32]),
            now_ms,
            Sequence(1),
        )
    }

    #[test]
    fn winner_is_symmetric() {
        let a = record(1, b"a", 5_000);
        let b = record(2, b"b", 5_000);
        let ab = choose_winner(&a, &b);
        let ba = choose_winner(&b, &a);
        assert_ne!(
            ab, ba,
            "swapping inputs must swap the label, not the outcome"
        );
        assert_eq!(ab, ConflictWinner::B);
    }

    #[test]
    fn conflict_copy_does_not_keep_the_extension_last() {
        let p = LogicalPath::new("src/foo.go").unwrap();
        let c = conflict_path(&p, &DeviceId::from_bytes([0xab; 32]), 42).unwrap();
        assert_eq!(c.as_str(), "src/foo.go.relay-conflict-abababab-42");
        assert!(is_conflict_copy(&c));
        assert!(!is_conflict_copy(&p));
    }
}
