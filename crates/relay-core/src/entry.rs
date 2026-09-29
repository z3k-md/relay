use serde::{Deserialize, Serialize};

use crate::ids::{DeviceId, MountId, ObjectId, Sequence, SpaceId};
use crate::path::LogicalPath;
use crate::version::VersionVector;

/// Canonical identity of a synchronized entry. Never an OS path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EntryKey {
    pub space: SpaceId,
    pub mount: MountId,
    pub path: LogicalPath,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
        }
    }
}

/// What an entry holds at one version. `Deleted` is a tombstone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryContent {
    File {
        object: ObjectId,
        size: u64,
        executable: bool,
    },
    Directory,
    Symlink {
        target: String,
    },
    Deleted,
}

impl EntryContent {
    pub fn kind(&self) -> Option<EntryKind> {
        match self {
            Self::File { .. } => Some(EntryKind::File),
            Self::Directory => Some(EntryKind::Directory),
            Self::Symlink { .. } => Some(EntryKind::Symlink),
            Self::Deleted => None,
        }
    }

    pub fn is_deleted(&self) -> bool {
        matches!(self, Self::Deleted)
    }

    pub fn object(&self) -> Option<ObjectId> {
        match self {
            Self::File { object, .. } => Some(*object),
            _ => None,
        }
    }

    /// Whether two versions would materialize identically.
    pub fn same_content(&self, other: &EntryContent) -> bool {
        self == other
    }
}

/// Filesystem metadata last observed for a materialized entry.
///
/// Used only to decide whether a file must be re-hashed. It is local to one
/// device and never replicated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatHint {
    pub size: u64,
    pub mtime_ns: i64,
    /// Inode on Unix, file index on Windows, when available.
    pub file_id: Option<u64>,
}

/// The current state of one entry in a device's index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryRecord {
    pub key: EntryKey,
    pub content: EntryContent,
    pub vector: VersionVector,
    /// Object this version was derived from. Kept from the first release so
    /// later three-way merges have a base to work with.
    pub parent_object: Option<ObjectId>,
    /// Local sequence number at which this version entered the index.
    pub sequence: Sequence,
    pub modified_by: DeviceId,
    /// Display only. Never used for ordering.
    pub modified_at_unix_ms: i64,
    pub stat: Option<StatHint>,
}

impl EntryRecord {
    /// Build the record for a new local write (create, modify or delete).
    ///
    /// The new vector descends from the previous one, including when the
    /// previous version was a tombstone, so a re-created file supersedes its
    /// own deletion everywhere.
    pub fn local_write(
        previous: Option<&EntryRecord>,
        key: EntryKey,
        content: EntryContent,
        stat: Option<StatHint>,
        device: DeviceId,
        now_unix_ms: i64,
        sequence: Sequence,
    ) -> EntryRecord {
        let mut vector = previous.map(|p| p.vector.clone()).unwrap_or_default();
        let now_secs = u64::try_from(now_unix_ms / 1000).unwrap_or(0);
        vector.bump(device, now_secs);
        EntryRecord {
            key,
            content,
            vector,
            parent_object: previous.and_then(|p| p.content.object()),
            sequence,
            modified_by: device,
            modified_at_unix_ms: now_unix_ms,
            stat,
        }
    }

    pub fn is_deleted(&self) -> bool {
        self.content.is_deleted()
    }
}
