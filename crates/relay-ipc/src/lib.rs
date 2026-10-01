//! Local IPC for a running Relay host.
//!
//! Transport is the `interprocess` crate's local sockets (Unix domain sockets
//! or Windows named pipes). The protocol is newline-delimited JSON.

mod client;
mod endpoint;
mod protocol;
mod server;

pub use client::{Client, Subscribe};
pub use endpoint::{Endpoint, home_token};
pub use protocol::{
    ActivityItem, AddMountParams, AddMountResult, FetchParams, Hello, HostKind, HostState, Idle,
    MountLive, PROTOCOL_VERSION, PairJoinParams, PairJoinResult, PairStartParams, PairStartResult,
    PairStatus, PeerLive, Request, RescanParams, RescanResult, RpcError, RpcErrorBody, ShareParams,
    Status, TransferDirection, TransferLive, Watching, decode_line, encode_line,
};
pub use server::{Handler, Server};

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid IPC message: {0}")]
    Codec(String),
    #[error("host protocol {found} is not supported (this client speaks {expected})")]
    ProtocolMismatch { found: u32, expected: u32 },
    #[error("{code}: {message}")]
    Remote { code: String, message: String },
}

impl IpcError {
    pub fn codec(err: impl std::fmt::Display) -> Self {
        Self::Codec(err.to_string())
    }

    pub fn from_rpc(err: RpcErrorBody) -> Self {
        Self::Remote {
            code: err.code,
            message: err.message,
        }
    }
}

#[cfg(test)]
mod tests;
