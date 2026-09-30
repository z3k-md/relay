use std::io;
use std::path::PathBuf;

use relay_core::ObjectId;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReplicaError {
    #[error("I/O error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("replica path is missing or not a directory: {}", .0.display())]
    NotADirectory(PathBuf),

    #[error("object id does not match content hash (expected {expected}, got {actual})")]
    ObjectIdMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },

    #[error("object {id} is corrupt (on-disk hash {actual})")]
    CorruptObject { id: ObjectId, actual: ObjectId },

    #[error("append sequence {got} is not the next after {expected}")]
    SequenceGap { expected: u64, got: u64 },

    #[error("replay of sequence {sequence} has a different payload")]
    SequenceConflict { sequence: u64 },

    #[error("invalid entry log: {0}")]
    CorruptLog(String),

    #[error("invalid ack file: {0}")]
    CorruptAck(String),

    #[error(transparent)]
    Proto(#[from] relay_proto::ProtoError),
}

impl ReplicaError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
