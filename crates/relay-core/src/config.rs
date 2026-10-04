//! Configuration changes as data.
//!
//! Every caller that edits spaces, mounts, shares, or materialization rules
//! describes the edit as a [`ConfigChange`]: the CLI, the desktop app, host
//! IPC, and later a peer that manages this device. A running host applies the
//! change on its sync loop so live sessions survive; with no host running the
//! engine writes it directly. Both paths share one implementation.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{Mount, Space};

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
}

impl ConfigChange {
    /// The space this change touches, by name.
    pub fn space(&self) -> &str {
        match self {
            Self::CreateSpace { space }
            | Self::DeleteSpace { space }
            | Self::JoinSpace { space, .. }
            | Self::AddMount { space, .. }
            | Self::RemoveMount { space, .. }
            | Self::Share { space, .. }
            | Self::Unshare { space, .. }
            | Self::MaterializeAdd { space, .. }
            | Self::MaterializeRemove { space, .. } => space,
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
        assert_eq!(change.space(), "Projects");
    }
}
