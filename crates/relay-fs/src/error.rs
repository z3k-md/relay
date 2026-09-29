use std::io;
use std::path::PathBuf;

use relay_core::{MountId, ObjectId};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("mount root {0} does not exist")]
    MountRootMissing(PathBuf),

    #[error("path {0} is not a directory")]
    NotADirectory(PathBuf),

    #[error("mount marker missing at {0}")]
    MarkerMissing(PathBuf),

    #[error("mount marker at {path} is for {found_mount}, expected {expected_mount}")]
    MarkerMismatch {
        path: PathBuf,
        expected_mount: MountId,
        found_mount: MountId,
    },

    #[error("mount marker at {path} is invalid: {message}")]
    MarkerInvalid { path: PathBuf, message: String },

    #[error("destination {0} changed while materializing")]
    DestinationChanged(PathBuf),

    #[error("hash mismatch at {path}: expected {expected}, got {actual}")]
    HashMismatch {
        path: PathBuf,
        expected: ObjectId,
        actual: ObjectId,
    },

    #[error("path {0} is not valid UTF-8")]
    NonUtf8(PathBuf),

    #[error("invalid name at {os_path}: {reason}")]
    InvalidName { os_path: PathBuf, reason: String },

    #[error("path {path} cannot be represented on this OS: {reason}")]
    Unrepresentable { path: String, reason: String },

    #[error("refusing to write through {path}: {reason}")]
    UnsafeAncestor { path: PathBuf, reason: String },

    #[error("invalid ignore pattern {pattern:?}: {message}")]
    InvalidPattern { pattern: String, message: String },

    #[error("filesystem error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl FsError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
