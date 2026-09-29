use std::io;
use std::net::SocketAddr;

/// Failures from [`crate::start`] and TLS/endpoint setup.
///
/// Bind errors (port in use, permission denied, …) are returned here. If the
/// accept loop dies after a successful start, that is reported as
/// [`crate::NetEvent::ListenFailed`] instead.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("failed to bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("tls configuration: {0}")]
    Tls(String),
    #[error("identity: {0}")]
    Identity(#[from] relay_crypto::CryptoError),
    #[error("object store: {0}")]
    Store(#[from] relay_store::StoreError),
    #[error("runtime: {0}")]
    Runtime(String),
    #[error("{0}")]
    Other(String),
}
