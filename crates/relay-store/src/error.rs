use std::io;
use std::path::PathBuf;

use relay_core::ObjectId;
use thiserror::Error;

/// Failures from the content-addressed object store.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Source file metadata changed while we were hashing it (or never matched
    /// the caller's expected hint). The temp copy is discarded.
    #[error("source file changed while reading {}", path.display())]
    SourceChanged { path: PathBuf },

    #[error("object not found: {0}")]
    NotFound(ObjectId),

    #[error("object {id} is corrupt (on-disk hash {actual})")]
    Corrupt { id: ObjectId, actual: ObjectId },
}
