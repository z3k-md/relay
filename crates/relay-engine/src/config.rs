//! Applying a [`ConfigChange`] to this device's database and folders.
//!
//! [`Engine::apply_config`] is the only place that maps a change onto engine
//! calls. The sync loop runs it and then performs live follow-ups (watchers,
//! offers, index requests); a direct write with no host running calls it
//! alone.

use std::fs;
use std::io;
use std::path::Path;

use relay_core::{ConfigApplied, ConfigChange, Device, MOUNT_MARKER, MountId};
use relay_fs::MountMarker;

use crate::Engine;
use crate::error::EngineError;
use crate::materialize::MaterializationMode;

impl Engine {
    pub fn apply_config(&mut self, change: &ConfigChange) -> Result<ConfigApplied, EngineError> {
        Ok(match change {
            ConfigChange::CreateSpace { space } => ConfigApplied::Space {
                space: self.create_space(space)?,
            },
            ConfigChange::DeleteSpace { space } => {
                self.delete_space(space)?;
                ConfigApplied::Done
            }
            ConfigChange::JoinSpace {
                space, from_peer, ..
            } => ConfigApplied::Space {
                space: self.join_space(space, from_peer)?,
            },
            ConfigChange::AddMount {
                space,
                mount,
                path,
                includes,
                excludes,
            } => {
                let config = self.add_mount(space, mount, path, includes, excludes)?;
                ConfigApplied::Mount {
                    mount: config.mount,
                    path: config.local_path,
                }
            }
            ConfigChange::RemoveMount { space, mount } => {
                self.remove_mount(space, mount)?;
                ConfigApplied::Done
            }
            ConfigChange::Share { space, peer } => {
                self.share(space, peer)?;
                ConfigApplied::Done
            }
            ConfigChange::Unshare { space, peer } => {
                self.unshare(space, peer)?;
                ConfigApplied::Done
            }
            ConfigChange::MaterializeAdd {
                space,
                name,
                mode,
                selectors,
            } => {
                self.materialize_add(space, name, mode, selectors)?;
                ConfigApplied::Done
            }
            ConfigChange::MaterializeRemove { space, name } => {
                self.materialize_remove(space, name)?;
                ConfigApplied::Done
            }
            ConfigChange::SetFolderMode {
                space,
                mount,
                path,
                mode,
            } => {
                let mode = mode
                    .as_deref()
                    .map(MaterializationMode::parse)
                    .transpose()?;
                self.set_folder_mode(space, mount, path, mode)?;
                ConfigApplied::Done
            }
            ConfigChange::AddPeer {
                peer,
                id,
                addresses,
            } => {
                let added = self.add_peer(peer, *id, addresses)?;
                ConfigApplied::Peer {
                    device: Device {
                        id: added.id,
                        name: added.name,
                    },
                }
            }
            ConfigChange::RemovePeer { peer } => {
                self.remove_peer(peer)?;
                ConfigApplied::Done
            }
            ConfigChange::RevokePeer { peer } => {
                self.revoke_peer(peer)?;
                ConfigApplied::Done
            }
            ConfigChange::SetPeerManage { peer, allowed } => {
                self.set_peer_manage(peer, *allowed)?;
                ConfigApplied::Done
            }
            ConfigChange::GroupCreate { group } => {
                self.group_create(group)?;
                ConfigApplied::Done
            }
            ConfigChange::GroupAdd { group, member } => {
                self.group_add(group, member)?;
                ConfigApplied::Done
            }
            ConfigChange::GroupRemove { group, member } => {
                self.group_remove_member(group, member)?;
                ConfigApplied::Done
            }
            ConfigChange::GroupDelete { group } => {
                self.group_delete(group)?;
                ConfigApplied::Done
            }
            ConfigChange::PolicyAdd {
                space,
                name,
                selectors,
                peers,
                groups,
            } => {
                self.policy_add(space, name, selectors, peers, groups)?;
                ConfigApplied::Done
            }
            ConfigChange::PolicyRemove { space, name } => {
                self.policy_remove(space, name)?;
                ConfigApplied::Done
            }
            ConfigChange::DecideDeleteHold {
                space,
                mount,
                peer,
                decision,
            } => ConfigApplied::Holds {
                decided: self.decide_delete_hold(
                    space,
                    mount.as_deref(),
                    peer.as_deref(),
                    *decision,
                )?,
            },
        })
    }

    /// Stop syncing a mount on this device and return it to the unattached
    /// state. Files in the folder are left alone (§48.6).
    pub fn remove_mount(&mut self, space: &str, mount: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let (_, config) = self.lookup_mount(space, mount)?;
        let root = config.local_path.ok_or(EngineError::MountNotLocal)?;
        let id = config.mount.id;
        self.db
            .transaction(|repo| repo.detach_local_mount(id))
            .map_err(EngineError::from_db)?;
        // After the commit: a failed write leaves the mount fully attached. A
        // marker that survives here is adoptable (`can_adopt_leftover_marker`).
        if let Err(err) = remove_marker(&root, id) {
            tracing::warn!(path = %root.display(), error = %err, "could not remove mount marker");
        }
        Ok(())
    }

    /// Forget a space on this device. Refused while any mount is attached.
    pub fn delete_space(&mut self, space: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let space_rec = self
            .db
            .repo()
            .space_by_name(space)?
            .ok_or_else(|| EngineError::UnknownSpace(space.to_owned()))?;
        let attached: Vec<String> = self
            .db
            .repo()
            .list_mounts(Some(space_rec.id))?
            .into_iter()
            .filter(|cfg| cfg.local_path.is_some())
            .map(|cfg| cfg.mount.name)
            .collect();
        if !attached.is_empty() {
            return Err(EngineError::SpaceHasAttachedMounts {
                space: space.to_owned(),
                mounts: attached,
            });
        }
        self.db
            .transaction(|repo| repo.delete_space(space_rec.id))
            .map_err(EngineError::from_db)
    }
}

/// Delete `root`'s marker if it names `mount`. A marker for another mount, or
/// none, is left as found.
fn remove_marker(root: &Path, mount: MountId) -> io::Result<()> {
    if MountMarker::verify(root, mount).is_err() {
        return Ok(());
    }
    match fs::remove_file(root.join(MOUNT_MARKER)) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}
