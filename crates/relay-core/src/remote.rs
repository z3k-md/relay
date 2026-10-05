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
/// Largest file a read-only copy fetches (D41). Bigger files sync instead.
pub const READ_COPY_MAX: u64 = 256 * 1024 * 1024;

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
    /// Where a file is: its folder, its name, and the mount it is in, if any.
    Locate {
        path: String,
    },
    /// Index one path of a mount now, ahead of a full scan of the mount, so
    /// a file being opened elsewhere gets its index row first.
    ScanFirst {
        space: String,
        mount: String,
        /// Inside the mount, `/`-separated.
        path: String,
    },
    /// Space on disk taken by each folder directly inside `path`, counted
    /// in the background on the answering device (D42). Ask again for
    /// progress until [`FolderSizes::done`].
    FolderSizes {
        path: String,
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
    Located { located: Located },
    Done,
    FolderSizes { sizes: FolderSizes },
}

/// Progress on the sizes of the folders inside one folder (D42).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderSizes {
    /// Canonical path of the folder asked about.
    pub path: String,
    pub folders: Vec<FolderSize>,
    /// Every folder is counted.
    pub done: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderSize {
    /// Same as the folder's [`DirEntry::path`] in a listing.
    pub path: String,
    /// Space allocated on disk, so far if not `done`.
    pub bytes: u64,
    /// Files counted, so far if not `done`.
    pub files: u64,
    pub done: bool,
}

/// A read-only copy that arrived (D41).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopiedFile {
    pub size: u64,
    pub modified_ms: Option<i64>,
}

/// A file found by [`RemoteCall::Locate`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Located {
    /// The folder holding it, canonical, in the answering device's format.
    pub folder: String,
    pub name: String,
    pub size: u64,
    /// The mount it is in, with its path inside the mount.
    pub mount: Option<MountedPath>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountedPath {
    pub space: String,
    pub mount: String,
    /// Inside the mount, `/`-separated.
    pub path: String,
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
    /// This folder and every folder above it, outermost first, for a path
    /// bar: the caller never splits paths itself. Empty from older devices.
    #[serde(default)]
    pub ancestors: Vec<RemoteRoot>,
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
    /// A file's length in bytes.
    pub size: Option<u64>,
    /// Space a file takes on disk: allocated blocks, so less than `size`
    /// for compressed, sparse, or cloud-only files (D42). `None` from
    /// devices that predate it.
    #[serde(default)]
    pub disk_size: Option<u64>,
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
    /// The answering device keeps that from managing devices: a credential
    /// store, or the contents of a file outside its synced folders (D45).
    Protected,
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
            Self::Protected => "protected",
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
            "protected" => Self::Protected,
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
