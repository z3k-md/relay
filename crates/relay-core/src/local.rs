//! Turning "what the index says" plus "what is on disk now" into a change.

use crate::entry::{EntryContent, EntryRecord, StatHint};

/// An entry as found on disk by a scan, after hashing if needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// Never `EntryContent::Deleted`; absence is represented by `None`.
    pub content: EntryContent,
    pub stat: Option<StatHint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalChange {
    Unchanged,
    /// Same content, different filesystem metadata (e.g. `touch`). Update the
    /// stat hint without creating a new version.
    StatOnly,
    Created,
    Modified,
    Deleted,
}

impl LocalChange {
    pub fn creates_version(self) -> bool {
        matches!(self, Self::Created | Self::Modified | Self::Deleted)
    }
}

pub fn derive_local_change(
    previous: Option<&EntryRecord>,
    observed: Option<&Observation>,
) -> LocalChange {
    let live_previous = previous.filter(|p| !p.is_deleted());
    match (live_previous, observed) {
        (None, None) => LocalChange::Unchanged,
        (None, Some(_)) => LocalChange::Created,
        (Some(_), None) => LocalChange::Deleted,
        (Some(prev), Some(obs)) => {
            if !prev.content.same_content(&obs.content) {
                LocalChange::Modified
            } else if prev.stat != obs.stat {
                LocalChange::StatOnly
            } else {
                LocalChange::Unchanged
            }
        }
    }
}

/// Whether a file's bytes must be hashed again, or the previous object can be
/// trusted because size, mtime, ctime and file identity are all unchanged.
pub fn needs_rehash(previous: Option<&EntryRecord>, stat: &StatHint) -> bool {
    match previous {
        Some(prev) => {
            !matches!(prev.content, EntryContent::File { .. }) || prev.stat != Some(*stat)
        }
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKey;
    use crate::ids::{DeviceId, MountId, ObjectId, Sequence, SpaceId};
    use crate::path::LogicalPath;
    use crate::version::VectorOrdering;

    fn key() -> EntryKey {
        EntryKey {
            space: SpaceId::new(),
            mount: MountId::new(),
            path: LogicalPath::new("a.lua").unwrap(),
        }
    }

    fn file(data: &[u8]) -> EntryContent {
        EntryContent::File {
            object: ObjectId::of(data),
            size: data.len() as u64,
            executable: false,
        }
    }

    fn stat(mtime: i64) -> StatHint {
        StatHint {
            size: 1,
            mtime_ns: mtime,
            file_id: Some(7),
            ctime_ns: None,
        }
    }

    fn obs(data: &[u8], mtime: i64) -> Observation {
        Observation {
            content: file(data),
            stat: Some(stat(mtime)),
        }
    }

    fn write(prev: Option<&EntryRecord>, content: EntryContent, seq: u64) -> EntryRecord {
        EntryRecord::local_write(
            prev,
            key(),
            content,
            Some(stat(1)),
            DeviceId::from_bytes([1; 32]),
            1_000,
            Sequence(seq),
        )
    }

    #[test]
    fn classifies_lifecycle() {
        assert_eq!(derive_local_change(None, None), LocalChange::Unchanged);
        assert_eq!(
            derive_local_change(None, Some(&obs(b"x", 1))),
            LocalChange::Created
        );

        let v1 = write(None, file(b"x"), 1);
        assert_eq!(
            derive_local_change(Some(&v1), Some(&obs(b"x", 1))),
            LocalChange::Unchanged
        );
        assert_eq!(
            derive_local_change(Some(&v1), Some(&obs(b"x", 2))),
            LocalChange::StatOnly
        );
        assert_eq!(
            derive_local_change(Some(&v1), Some(&obs(b"y", 2))),
            LocalChange::Modified
        );
        assert_eq!(derive_local_change(Some(&v1), None), LocalChange::Deleted);

        let tomb = write(Some(&v1), EntryContent::Deleted, 2);
        assert_eq!(
            derive_local_change(Some(&tomb), None),
            LocalChange::Unchanged
        );
        assert_eq!(
            derive_local_change(Some(&tomb), Some(&obs(b"x", 3))),
            LocalChange::Created
        );
    }

    #[test]
    fn each_write_descends_from_the_previous_including_tombstones() {
        let v1 = write(None, file(b"x"), 1);
        let tomb = write(Some(&v1), EntryContent::Deleted, 2);
        let again = write(Some(&tomb), file(b"x"), 3);
        assert_eq!(tomb.vector.compare(&v1.vector), VectorOrdering::Dominates);
        assert_eq!(
            again.vector.compare(&tomb.vector),
            VectorOrdering::Dominates
        );
        assert_eq!(tomb.parent_object, v1.content.object());
        assert_eq!(again.parent_object, None);
    }

    #[test]
    fn rehash_is_skipped_only_for_identical_stat() {
        let v1 = write(None, file(b"x"), 1);
        assert!(!needs_rehash(Some(&v1), &stat(1)));
        assert!(needs_rehash(Some(&v1), &stat(2)));
        assert!(needs_rehash(None, &stat(1)));
        let mut other = stat(1);
        other.ctime_ns = Some(99);
        assert!(needs_rehash(Some(&v1), &other));
    }
}
