use thiserror::Error;

use relay_core::Sequence;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("database schema version {found} is newer than this binary supports ({supported})")]
    SchemaTooNew { found: u32, supported: u32 },

    #[error("database schema version {found} is older than this binary requires ({supported})")]
    SchemaTooOld { found: u32, supported: u32 },

    #[error("local device is already initialized")]
    AlreadyInitialized,

    #[error("local device is not initialized")]
    NotInitialized,

    #[error("duplicate name {0:?}")]
    DuplicateName(String),

    #[error("duplicate sequence {0}")]
    DuplicateSequence(Sequence),

    #[error("entry space does not match the mount's space")]
    SpaceMismatch,

    #[error("path is not valid UTF-8")]
    NonUtf8Path,

    #[error("not found")]
    NotFound,

    #[error("integer does not fit in SQLite INTEGER")]
    IntegerOverflow,

    #[error("corrupt database: {0}")]
    Corrupt(String),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
