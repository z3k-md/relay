use std::io;
use std::path::PathBuf;

use relay_core::CoreError;
use relay_db::DbError;
use relay_fs::FsError;
use relay_policy::PolicyError;
use relay_store::StoreError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("relay is not initialized; run `relay init`")]
    NotInitialized,

    #[error("relay is already initialized")]
    AlreadyInitialized,

    #[error("unknown space {0:?}")]
    UnknownSpace(String),

    #[error("unknown mount {mount:?} in space {space:?}")]
    UnknownMount { space: String, mount: String },

    #[error("mount has no local path on this device")]
    MountNotLocal,

    #[error("{0}")]
    InvalidName(#[from] CoreError),

    #[error("path {} is not a directory", .0.display())]
    PathNotADirectory(PathBuf),

    #[error("mount path overlaps existing mount {}", existing.display())]
    OverlappingMount { existing: PathBuf },

    #[error("mount path overlaps the Relay home directory")]
    OverlapsRelayHome,

    #[error("path already has a mount marker: {}", path.display())]
    MountAlreadyClaimed { path: PathBuf },

    #[error("refusing to delete {deletions} of {live} live entries")]
    MassDeleteRefused { deletions: usize, live: usize },

    #[error("unknown version")]
    UnknownVersion,

    #[error("restore is not supported: {0}")]
    RestoreUnsupported(String),

    #[error("destination changed: {}", .0.display())]
    DestinationChanged(PathBuf),

    #[error(transparent)]
    Db(#[from] DbError),

    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Fs(#[from] FsError),

    #[error(transparent)]
    Policy(#[from] PolicyError),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

impl EngineError {
    pub(crate) fn from_db(err: DbError) -> Self {
        match err {
            DbError::AlreadyInitialized => Self::AlreadyInitialized,
            DbError::NotInitialized => Self::NotInitialized,
            other => Self::Db(other),
        }
    }
}
