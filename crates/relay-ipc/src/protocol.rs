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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub state: HostState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    pub peers: Vec<PeerLive>,
    pub mounts: Vec<MountLive>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerLive {
    pub id: String,
    pub name: String,
    pub connected_at_ms: u64,
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

pub fn encode_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(value)
}

pub fn decode_line<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
}
