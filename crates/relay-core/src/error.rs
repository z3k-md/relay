use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CoreError {
    #[error("invalid logical path {path:?}: {reason}")]
    InvalidPath { path: String, reason: &'static str },

    #[error("invalid identifier {value:?}: {reason}")]
    InvalidId { value: String, reason: &'static str },

    #[error("invalid name {name:?}: {reason}")]
    InvalidName { name: String, reason: &'static str },

    #[error("invalid pairing code {value:?}: {reason}")]
    InvalidPairingCode { value: String, reason: &'static str },
}
