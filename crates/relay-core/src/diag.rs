//! What a device reports about its network for a diagnostics bundle (D56).
//! `relay-net` fills these in; the daemon writes them into the bundle.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetReport {
    /// The bound QUIC socket.
    pub listen: String,
    /// The public address STUN reported, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reflexive: Option<String>,
    /// `host:port` dialed after every direct address fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    /// mDNS runs (off for loopback-only nodes).
    pub lan_discovery: bool,
    /// Every interface address, loopback and VPNs included: `en0 192.168.1.5`.
    pub interfaces: Vec<String>,
    /// Addresses this device advertises to peers.
    pub advertised: Vec<String>,
    pub peers: Vec<PeerNetReport>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerNetReport {
    pub id: String,
    /// Stored addresses, in the order they are dialed.
    pub addresses: Vec<String>,
    /// LAN addresses mDNS resolved for it, tried first.
    pub discovered: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ConnReport>,
    /// The last dial round that reached nothing, cleared on connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_dial_failure: Option<DialFailure>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnReport {
    /// Same on both ends of the connection; log lines carry it as `conn`.
    pub conn_id: String,
    pub name: String,
    pub remote: String,
    pub we_dialed: bool,
    pub since_ms: u64,
    pub rtt_ms: u64,
    /// The peer's clock minus this one's, measured at `Hello` (D56).
    /// `None` when the peer is too old to send its time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_offset_ms: Option<i64>,
    pub version: String,
    pub features: u64,
    /// The peer lets this device manage it.
    pub grants_us: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialFailure {
    pub at_ms: u64,
    /// One `address: outcome` per attempt, `; `-separated.
    pub attempts: String,
}
