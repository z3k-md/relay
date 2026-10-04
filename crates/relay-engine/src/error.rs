use std::io;
use std::path::PathBuf;

use relay_core::{CoreError, LogicalPath};
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

    #[error(
        "this Relay home predates device keys or the key does not match the database; re-init with a fresh home (RELAY_HOME)"
    )]
    StaleIdentity { home: PathBuf },

    #[error("unknown peer {0:?}")]
    UnknownPeer(String),

    #[error("peer {0:?} already exists")]
    DuplicatePeer(String),

    #[error("unknown device group {0:?}")]
    UnknownGroup(String),

    #[error("unknown replication policy {0:?}")]
    UnknownPolicy(String),

    #[error("policy needs at least one selector and one target")]
    EmptyPolicy,

    #[error("unknown materialization rule {0:?}")]
    UnknownMaterialization(String),

    #[error("materialization rule needs at least one selector")]
    EmptyMaterialization,

    #[error("materialization rule {0:?} already exists")]
    DuplicateMaterialization(String),

    #[error("unknown materialization mode {0:?}")]
    UnknownMaterializationMode(String),

    #[error("{path} is {mode}; fetch and evict only work for demand")]
    NotDemand { path: String, mode: String },

    #[error("no entry for {0}")]
    UnknownEntry(String),

    #[error("{0} is deleted")]
    EntryDeleted(String),

    #[error("object not available locally or in the mailbox")]
    ObjectUnavailable,

    #[error("{path} does not match the index")]
    EvictMismatch { path: String },

    #[error("{0} is not materialized")]
    NotMaterialized(String),

    #[error("space {name:?} already exists with a different id")]
    SpaceIdConflict { name: String },

    #[error("no offer for {0:?} from that peer")]
    UnknownOffer(String),

    #[error("mount {mount:?} in space {space:?} is already attached")]
    MountAlreadyAttached { space: String, mount: String },

    #[error(transparent)]
    Crypto(#[from] relay_crypto::CryptoError),

    #[error(transparent)]
    Proto(#[from] relay_proto::ProtoError),

    #[error("unknown space {0:?}")]
    UnknownSpace(String),

    #[error("unknown mount {mount:?} in space {space:?}")]
    UnknownMount { space: String, mount: String },

    #[error("mount has no local path on this device")]
    MountNotLocal,

    #[error("space {space:?} still has attached mounts: {}", mounts.join(", "))]
    SpaceHasAttachedMounts { space: String, mounts: Vec<String> },

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

    #[error("another relay process is using {}", .home.display())]
    Busy { home: PathBuf },

    #[error("Relay is syncing {} right now", .home.display())]
    Running { home: PathBuf },

    #[error("relay home is open read-only")]
    ReadOnly,

    #[error("relay address must be host:port")]
    BadRelayAddress,

    #[error("peer {0} is revoked")]
    PeerRevoked(String),

    #[error("recovery key was not accepted")]
    RecoveryRejected,

    #[error("this home has no encryption key")]
    NoBoxKey,

    #[error("index changed concurrently at {path}")]
    ConcurrentModification { path: LogicalPath },

    #[error("refusing to delete {deletions} of {live} live entries")]
    MassDeleteRefused { deletions: usize, live: usize },

    #[error("unknown version")]
    UnknownVersion,

    #[error("restore is not supported: {0}")]
    RestoreUnsupported(String),

    #[error("{0} is not a live conflict copy")]
    NotAConflictCopy(LogicalPath),

    #[error("cannot resolve a directory conflict copy at {0}")]
    DirectoryConflict(LogicalPath),

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

    #[error("{0}")]
    Replica(String),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// The watch loop paused a scan to apply a mount or share. Callers other
    /// than the watch loop do not request this.
    #[doc(hidden)]
    #[error("scan paused")]
    Interrupted,
}

impl EngineError {
    /// Stable snake_case class for callers across a process boundary (IPC,
    /// later a managing peer). Messages are for people; codes are for code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownPeer(_)
            | Self::UnknownGroup(_)
            | Self::UnknownPolicy(_)
            | Self::UnknownMaterialization(_)
            | Self::UnknownEntry(_)
            | Self::UnknownOffer(_)
            | Self::UnknownSpace(_)
            | Self::UnknownMount { .. }
            | Self::UnknownVersion => "not_found",
            Self::DuplicatePeer(_)
            | Self::DuplicateMaterialization(_)
            | Self::SpaceIdConflict { .. }
            | Self::MountAlreadyAttached { .. }
            | Self::MountAlreadyClaimed { .. }
            | Self::Db(DbError::DuplicateName(_)) => "already_exists",
            Self::EmptyPolicy
            | Self::EmptyMaterialization
            | Self::UnknownMaterializationMode(_)
            | Self::InvalidName(_)
            | Self::PathNotADirectory(_)
            | Self::BadRelayAddress
            | Self::Policy(_) => "invalid",
            Self::NotDemand { .. }
            | Self::EntryDeleted(_)
            | Self::EvictMismatch { .. }
            | Self::NotMaterialized(_)
            | Self::MountNotLocal
            | Self::SpaceHasAttachedMounts { .. }
            | Self::OverlappingMount { .. }
            | Self::OverlapsRelayHome
            | Self::PeerRevoked(_)
            | Self::ConcurrentModification { .. }
            | Self::DestinationChanged(_)
            | Self::NotAConflictCopy(_)
            | Self::DirectoryConflict(_)
            | Self::RestoreUnsupported(_) => "precondition",
            Self::MassDeleteRefused { .. } => "mass_delete_refused",
            Self::Busy { .. } | Self::Running { .. } | Self::ReadOnly => "busy",
            Self::ObjectUnavailable => "unavailable",
            Self::NotInitialized => "not_initialized",
            _ => "failed",
        }
    }

    pub(crate) fn from_db(err: DbError) -> Self {
        match err {
            DbError::AlreadyInitialized => Self::AlreadyInitialized,
            DbError::NotInitialized => Self::NotInitialized,
            other => Self::Db(other),
        }
    }
}
