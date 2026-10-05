//! Deterministic conflict resolution.
//!
//! Every device must reach the same outcome from the same two versions, or the
//! "resolved" entry stays divergent. Nothing here may depend on which device is
//! doing the computing or on local wall-clock time.

use crate::entry::{EntryContent, EntryRecord};
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
///
/// Two versions from the same device with the same counter (`Diverged`, D2:
/// a reused counter after a database restore) rank by content, so both
/// devices still keep the same version at the path and copy the other.
pub fn choose_winner(a: &EntryRecord, b: &EntryRecord) -> ConflictWinner {
    let rank = |r: &EntryRecord| {
        (
            r.vector.get(&r.modified_by),
            r.modified_by,
            content_rank(&r.content),
        )
    };
    if rank(a) >= rank(b) {
        ConflictWinner::A
    } else {
        ConflictWinner::B
    }
}

/// A total order over content, used only to break otherwise equal ranks.
fn content_rank(content: &EntryContent) -> (u8, Vec<u8>) {
    match content {
        EntryContent::Deleted => (0, Vec::new()),
        EntryContent::Symlink { target } => (1, target.as_bytes().to_vec()),
        EntryContent::File { object, .. } => (2, object.as_bytes().to_vec()),
        EntryContent::Directory => (3, Vec::new()),
    }
}

/// Winner for a repository-level group: the version whose `modified_by`
/// [`DeviceId`] is greater. Same writer falls back to [`choose_winner`].
///
/// Device-id rank is independent of per-file counters (which are ~unix seconds)
/// so every mutable file in one `.git` directory is awarded to the same device.
pub fn choose_group_winner(a: &EntryRecord, b: &EntryRecord) -> ConflictWinner {
    if a.modified_by != b.modified_by {
        if a.modified_by > b.modified_by {
            ConflictWinner::A
        } else {
            ConflictWinner::B
        }
    } else {
        choose_winner(a, b)
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

/// Inverse of [`conflict_path`]: strip [`CONFLICT_MARKER`] and the device/counter
/// suffix from the file name. `None` if this is not a conflict copy.
pub fn original_path(copy: &LogicalPath) -> Option<LogicalPath> {
    let name = copy.file_name();
    let idx = name.find(CONFLICT_MARKER)?;
    if idx == 0 {
        return None;
    }
    let orig_name = &name[..idx];
    match copy.parent() {
        Some(parent) => parent.join(orig_name).ok(),
        None => LogicalPath::new(orig_name).ok(),
    }
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

    #[test]
    fn original_path_round_trips_conflict_path() {
        let cases = [
            "src/foo.go",
            "foo.go",
            "repo/.git/refs/heads/main",
            "repo/.git/index",
            "dir/Name.ext",
        ];
        let device = DeviceId::from_bytes([0xab; 32]);
        for raw in cases {
            let original = LogicalPath::new(raw).unwrap();
            let copy = conflict_path(&original, &device, 17).unwrap();
            assert_eq!(original_path(&copy).as_ref(), Some(&original), "{copy}");
            assert!(original_path(&original).is_none(), "{original}");
        }
        assert!(original_path(&LogicalPath::new("src/foo.go").unwrap()).is_none());
    }

    #[test]
    fn group_winner_is_symmetric() {
        let a = record(1, b"a", 5_000);
        let b = record(2, b"b", 5_000);
        let ab = choose_group_winner(&a, &b);
        let ba = choose_group_winner(&b, &a);
        assert_ne!(
            ab, ba,
            "swapping inputs must swap the label, not the outcome"
        );
        assert_eq!(ab, ConflictWinner::B);
    }

    #[test]
    fn diverged_same_writer_is_symmetric() {
        // Same device, same counter, different bytes: a reused counter.
        let a = record(7, b"a", 5_000);
        let mut b = record(7, b"b", 5_000);
        b.modified_by = a.modified_by;
        b.vector = a.vector.clone();
        let ab = choose_winner(&a, &b);
        let ba = choose_winner(&b, &a);
        assert_ne!(
            ab, ba,
            "swapping inputs must swap the label, not the outcome"
        );
        assert_eq!(choose_group_winner(&a, &b), ab);
        assert_eq!(choose_group_winner(&b, &a), ba);
    }

    #[test]
    fn group_winner_falls_back_when_same_device() {
        let a = record(7, b"a", 9_000);
        let mut b = record(7, b"b", 3_000);
        b.modified_by = a.modified_by;
        assert_eq!(choose_winner(&a, &b), ConflictWinner::A);
        assert_eq!(choose_group_winner(&a, &b), ConflictWinner::A);
        assert_eq!(choose_group_winner(&b, &a), ConflictWinner::B);
    }
}
