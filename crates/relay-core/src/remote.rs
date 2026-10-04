//! Calls one device makes on another it is allowed to manage (D37).
//!
//! These are the domain types. `relay-proto` maps them onto the wire, the
//! daemon answers them, and local IPC and the desktop app carry them as JSON.
//!
//! Paths are opaque strings in the answering device's native form. The caller
//! only shows them and sends them back; it never parses or joins them.

use serde::{Deserialize, Serialize};

use crate::config::{ConfigApplied, ConfigChange};

/// Most entries one listing returns. Larger folders page with a cursor.
pub const MAX_LISTING: u32 = 5_000;
/// Entries per page when the caller does not ask for a size.
pub const DEFAULT_LISTING: u32 = 2_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum RemoteCall {
    /// Places to start browsing: the home folder, drives, volumes.
    Roots,
    /// One page of a folder. `cursor` is the index of the first entry.
    ListDir {
        path: String,
        #[serde(default)]
        cursor: u32,
        #[serde(default)]
        limit: u32,
    },
    Stat {
        path: String,
    },
    /// Spaces and mounts on the answering device.
    Spaces,
    /// What making `path` a mount would mean: is it there, empty, writable,
    /// inside or around a mount, a cloud folder.
    Preview {
        path: String,
    },
    /// Create folder `name` inside `parent`. The answering device joins them.
    CreateDir {
        parent: String,
        name: String,
    },
    /// Apply a config change on the answering device (allowlisted by
    /// [`ConfigChange::allowed_remotely`]). Peers are named by device id.
    Apply {
        change: ConfigChange,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum RemoteReply {
    Roots { roots: Vec<RemoteRoot> },
    Listing { listing: DirListing },
    Stat { entry: DirEntry },
    Spaces { spaces: Vec<RemoteSpace> },
    Preview { preview: PathPreview },
    Created { entry: DirEntry },
    Applied { applied: ConfigApplied },
}

/// What making a folder a mount would mean.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathPreview {
    /// Canonical path, when it exists.
    pub path: Option<String>,
    pub exists: bool,
    pub is_dir: bool,
    /// Files below it, counted up to a bound.
    pub files: u64,
    pub bytes: u64,
    /// The count stopped at the bound; there are at least `files`.
    pub truncated: bool,
    /// The mount it is inside of or contains, which forbids a new one (§33).
    pub overlaps: Option<MountRef>,
    pub cloud_only: bool,
    /// This device could write there (checked by creating and removing a
    /// probe file).
    pub writable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRoot {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirListing {
    /// Canonical path of the folder listed.
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<DirEntry>,
    /// Pass back as `cursor` for the next page. `None` on the last page.
    pub next_cursor: Option<u32>,
    pub total: u32,
    /// The mount this folder is inside of (or is), if any.
    pub inside_mount: Option<MountRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub kind: DirEntryKind,
    pub size: Option<u64>,
    pub modified_ms: Option<i64>,
    /// Dot-file, or the hidden attribute on Windows.
    pub hidden: bool,
    /// A cloud placeholder (OneDrive Files On-Demand, iCloud): reading it
    /// downloads it, so syncing the folder would download everything.
    pub cloud_only: bool,
    /// This entry is a mount root.
    pub mount: Option<MountRef>,
    /// A mount root lies somewhere below this entry.
    pub contains_mount: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountRef {
    pub space: String,
    pub mount: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSpace {
    pub name: String,
    pub mounts: Vec<RemoteMount>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteMount {
    pub name: String,
    /// `None` when the mount is not attached on the answering device.
    pub path: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteErrorCode {
    /// The answering device has not granted this one manage access.
    Forbidden,
    /// The operating system refused (permissions, macOS privacy prompts).
    Denied,
    NotFound,
    Timeout,
    /// The other device runs a Relay that does not answer remote calls.
    Unsupported,
    Invalid,
    /// It already exists, or what is there now does not allow it (an
    /// overlapping mount, a space still in use).
    Conflict,
    Busy,
    /// The other device is not connected.
    Offline,
    Failed,
}

impl RemoteErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forbidden => "forbidden",
            Self::Denied => "denied",
            Self::NotFound => "not_found",
            Self::Timeout => "timeout",
            Self::Unsupported => "unsupported",
            Self::Invalid => "invalid",
            Self::Conflict => "conflict",
            Self::Busy => "busy",
            Self::Offline => "offline",
            Self::Failed => "failed",
        }
    }

    /// Unknown codes from a newer peer read as [`Self::Failed`].
    pub fn parse(value: &str) -> Self {
        match value {
            "forbidden" => Self::Forbidden,
            "denied" => Self::Denied,
            "not_found" => Self::NotFound,
            "timeout" => Self::Timeout,
            "unsupported" => Self::Unsupported,
            "invalid" => Self::Invalid,
            "conflict" | "already_exists" | "precondition" => Self::Conflict,
            "busy" => Self::Busy,
            "offline" => Self::Offline,
            _ => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct RemoteError {
    pub code: RemoteErrorCode,
    pub message: String,
}

impl RemoteError {
    pub fn new(code: RemoteErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub type RemoteResult = Result<RemoteReply, RemoteError>;
