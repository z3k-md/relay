//! Configuration changes as data.
//!
//! Every caller that edits spaces, mounts, shares, peers, groups, policies,
//! materialization rules, or held-delete decisions describes the edit as a
//! [`ConfigChange`]: the CLI, the desktop app, host
//! IPC, and later a peer that manages this device. A running host applies the
//! change on its sync loop so live sessions survive; with no host running the
//! engine writes it directly. Both paths share one implementation.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::DeviceId;
use crate::model::{Device, Mount, Space};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ConfigChange {
    CreateSpace {
        space: String,
    },
    /// Forget a space on this device. Refused while any of its mounts is
    /// attached here; files on disk are never touched.
    DeleteSpace {
        space: String,
    },
    /// Join a space `from_peer` offered and share it back. A running host
    /// waits up to `wait_ms` for that offer to arrive; a direct write does not.
    JoinSpace {
        space: String,
        from_peer: String,
        #[serde(default)]
        wait_ms: u64,
    },
    /// Create a mount, or attach a local folder to an offered one.
    AddMount {
        space: String,
        mount: String,
        path: PathBuf,
        #[serde(default)]
        includes: Vec<String>,
        #[serde(default)]
        excludes: Vec<String>,
    },
    /// Stop syncing a mount on this device. Files stay on disk; the local
    /// index and history for the mount are dropped.
    RemoveMount {
        space: String,
        mount: String,
    },
    Share {
        space: String,
        peer: String,
    },
    Unshare {
        space: String,
        peer: String,
    },
    MaterializeAdd {
        space: String,
        name: String,
        mode: String,
        selectors: Vec<String>,
    },
    MaterializeRemove {
        space: String,
        name: String,
    },
    /// What this device keeps for everything in one folder of a mount
    /// (`path` `""` is the whole mount). `mode` `None` drops the choice so the
    /// enclosing folder's applies. Replaces choices made inside that folder.
    SetFolderMode {
        space: String,
        mount: String,
        path: String,
        mode: Option<String>,
    },
    /// Trust a device by id (the manual path; pairing adds peers itself).
    AddPeer {
        peer: String,
        id: DeviceId,
        #[serde(default)]
        addresses: Vec<String>,
    },
    RemovePeer {
        peer: String,
    },
    /// Soft revoke: stop dialing and sharing; data already there stays.
    RevokePeer {
        peer: String,
    },
    /// Let `peer` manage this device (browse and set up sync), or stop (D37).
    SetPeerManage {
        peer: String,
        allowed: bool,
    },
    GroupCreate {
        group: String,
    },
    /// `member` is a peer name or this device's own name.
    GroupAdd {
        group: String,
        member: String,
    },
    GroupRemove {
        group: String,
        member: String,
    },
    GroupDelete {
        group: String,
    },
    PolicyAdd {
        space: String,
        name: String,
        selectors: Vec<String>,
        #[serde(default)]
        peers: Vec<String>,
        #[serde(default)]
        groups: Vec<String>,
    },
    PolicyRemove {
        space: String,
        name: String,
    },
    /// Decide held mass deletes (D22). `None` matches every mount or peer.
    DecideDeleteHold {
        space: String,
        #[serde(default)]
        mount: Option<String>,
        #[serde(default)]
        peer: Option<String>,
        decision: DeleteHoldDecision,
    },
}

impl ConfigChange {
    /// The space this change touches, by name. `None` for device-wide changes
    /// (peers and groups).
    pub fn space(&self) -> Option<&str> {
        match self {
            Self::CreateSpace { space }
            | Self::DeleteSpace { space }
            | Self::JoinSpace { space, .. }
            | Self::AddMount { space, .. }
            | Self::RemoveMount { space, .. }
            | Self::Share { space, .. }
            | Self::Unshare { space, .. }
            | Self::MaterializeAdd { space, .. }
            | Self::MaterializeRemove { space, .. }
            | Self::SetFolderMode { space, .. }
            | Self::PolicyAdd { space, .. }
            | Self::PolicyRemove { space, .. }
            | Self::DecideDeleteHold { space, .. } => Some(space),
            Self::AddPeer { .. }
            | Self::RemovePeer { .. }
            | Self::RevokePeer { .. }
            | Self::SetPeerManage { .. }
            | Self::GroupCreate { .. }
            | Self::GroupAdd { .. }
            | Self::GroupRemove { .. }
            | Self::GroupDelete { .. } => None,
        }
    }
}

/// What to do with a held mass delete from a peer (D22).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteHoldDecision {
    /// Let the deletes through.
    Apply,
    /// Keep the files here and put them back on the peer.
    Restore,
}

impl DeleteHoldDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Restore => "restore",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "apply" => Some(Self::Apply),
            "restore" => Some(Self::Restore),
            _ => None,
        }
    }
}

/// What a [`ConfigChange`] produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigApplied {
    /// `CreateSpace` and `JoinSpace`.
    Space {
        space: Space,
    },
    /// `AddMount`. `path` is the canonical local folder.
    Mount {
        mount: Mount,
        path: Option<PathBuf>,
    },
    /// `AddPeer`.
    Peer {
        device: Device,
    },
    /// `DecideDeleteHold`: how many holds the decision covered.
    Holds {
        decided: usize,
    },
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape_is_tagged_and_defaults_optional_fields() {
        let change: ConfigChange =
            serde_json::from_str(r#"{"op":"join_space","space":"Projects","from_peer":"laptop"}"#)
                .unwrap();
        assert_eq!(
            change,
            ConfigChange::JoinSpace {
                space: "Projects".into(),
                from_peer: "laptop".into(),
                wait_ms: 0,
            }
        );
        assert_eq!(change.space(), Some("Projects"));
    }
}
