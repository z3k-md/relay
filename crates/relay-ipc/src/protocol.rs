use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    Cli,
    Service,
    Desktop,
}

impl HostKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Service => "service",
            Self::Desktop => "desktop",
        }
    }
}

impl std::fmt::Display for HostKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for HostKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "cli" => Ok(Self::Cli),
            "service" => Ok(Self::Service),
            "desktop" => Ok(Self::Desktop),
            other => Err(format!(
                "unknown host kind {other:?} (expected cli, service, or desktop)"
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcErrorBody>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcErrorBody {
    pub code: String,
    pub message: String,
}

pub type RpcError = RpcErrorBody;

impl RpcErrorBody {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub relay_version: String,
    pub host: HostKind,
    pub pid: u32,
    pub started_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostState {
    Starting,
    Running,
    Paused,
    Reloading,
    Error,
}

impl HostState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Reloading => "reloading",
            Self::Error => "error",
        }
    }
}

/// Whether this host has finished the work it can do by itself.
///
/// `ready` is not cluster convergence. Another device may still be applying
/// what this one already pushed. `relay-sim wait-converged` checks every node.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Idle {
    /// Host is running and no scan or transfer is in flight.
    pub quiet: bool,
    /// Local sequences not yet appended to the mailbox.
    /// Absent when this device has no mailbox configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replica_behind: Option<u64>,
}

impl Idle {
    pub fn ready(&self) -> bool {
        self.quiet && self.replica_behind.unwrap_or(0) == 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub state: HostState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    pub peers: Vec<PeerLive>,
    pub mounts: Vec<MountLive>,
    /// Live indexing and peer transfers. Empty when nothing is in flight.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transfers: Vec<TransferLive>,
    /// Quiescence of this host. Omitted by older hosts; treat that as not idle.
    #[serde(default)]
    pub idle: Idle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Receive,
    Send,
    Index,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferLive {
    pub peer_id: String,
    pub peer_name: String,
    pub space: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount: Option<String>,
    pub direction: TransferDirection,
    pub files_done: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_total: Option<u64>,
    pub bytes_done: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
    pub bytes_per_sec: u64,
    pub started_at_ms: u64,
    pub retries: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerLive {
    pub id: String,
    pub name: String,
    pub connected_at_ms: u64,
    /// The peer's Relay answers remote calls (D37).
    #[serde(default)]
    pub supports_remote: bool,
    /// The peer lets this device manage it.
    #[serde(default)]
    pub manageable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Watching {
    Native,
    Poll,
    None,
}

impl Watching {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Poll => "poll",
            Self::None => "none",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountLive {
    pub space: String,
    pub mount: String,
    pub path: Option<PathBuf>,
    pub watching: Watching,
    pub last_scan_ms: Option<i64>,
    pub last_scan_summary: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityItem {
    pub at_ms: u64,
    pub kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RescanParams {
    #[serde(default)]
    pub space: Option<String>,
    #[serde(default)]
    pub mount: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RescanResult {
    pub queued: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchParams {
    pub space: String,
    pub mount: String,
    pub path: String,
}

/// `evict` reply: how many files this device dropped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvictResult {
    pub evicted: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairStartParams {
    #[serde(default)]
    pub share: Vec<String>,
    /// Let the device that joins manage this one (D37).
    #[serde(default)]
    pub allow_manage: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairStartResult {
    pub code: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairJoinParams {
    pub code: String,
    #[serde(default)]
    pub addr: Option<String>,
    /// Let the device showing the code manage this one (D37).
    #[serde(default)]
    pub allow_manage: bool,
}

/// One side of a folder pair: a device (`None` is this one, else a peer's
/// local name) and a folder in that device's own path format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderEnd {
    #[serde(default)]
    pub device: Option<String>,
    pub path: String,
}

/// Sync `source` with `dest` (D39).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderPairParams {
    pub source: FolderEnd,
    pub dest: FolderEnd,
    /// Create this folder inside `dest.path` and sync into it.
    #[serde(default)]
    pub create_dest: Option<String>,
    /// Space and mount name; defaults to the source folder's name.
    #[serde(default)]
    pub name: Option<String>,
    /// Subfolders of the source to leave out, `/`-separated and relative.
    #[serde(default)]
    pub excludes: Vec<String>,
    /// File name patterns to leave out anywhere (`~$*`, `.DS_Store`), on
    /// both devices.
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    /// The destination downloads files only when opened.
    #[serde(default)]
    pub dest_online_only: bool,
}

/// What [`FolderPairParams`] would do, before anything changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderPairPlan {
    pub space: String,
    pub source: relay_core::remote::PathPreview,
    /// `None` when the destination folder is to be created.
    pub dest: Option<relay_core::remote::PathPreview>,
    /// Reasons it cannot go ahead.
    pub problems: Vec<String>,
    /// Things to confirm before going ahead.
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderPairResult {
    pub space: String,
    pub source_path: String,
    pub dest_path: String,
}

/// Open a file on a paired device (D40).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRemoteParams {
    /// The peer's local name.
    pub peer: String,
    /// The file, in that device's path format.
    pub path: String,
    /// Where quick-open folders go; `~/Relay` when omitted.
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// Copy just this file, read-only, and set nothing up (D41).
    #[serde(default)]
    pub read_only: bool,
}

/// A remote file, now here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenedRemote {
    /// The local file, ready to open.
    pub path: PathBuf,
    /// The synced folder it is in. `None` for a read-only copy.
    pub synced: Option<relay_core::remote::MountRef>,
}

/// A folder synced only so its files could be opened here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickOpen {
    pub space: String,
    pub peer: String,
    /// The folder on the peer, in its path format.
    pub folder: String,
    pub local_path: Option<PathBuf>,
    pub created_at_ms: u64,
    pub last_opened_ms: u64,
    /// Relay set this folder up on the peer; removing it removes it there.
    /// Otherwise the peer already synced it and only stops sharing it.
    pub created_on_peer: bool,
}

/// A remote call on a paired device, by its local peer name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteParams {
    pub peer: String,
    pub call: relay_core::remote::RemoteCall,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairJoinResult {
    pub peer_name: String,
    pub peer_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum PairStatus {
    Idle,
    Waiting,
    Paired { peer_name: String, peer_id: String },
    Failed { reason: String },
    Expired,
}

pub fn encode_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(value)
}

pub fn decode_line<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
}
