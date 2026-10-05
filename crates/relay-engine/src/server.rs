//! Server role: an always-on device that keeps a copy of every space its
//! peers offer it (home server proposal, Stage 1).
//!
//! With a data folder set, the device joins each space offered by a peer
//! allowed to manage it
//! and attaches every mount of every joined space under
//! `<data>/<space>/<mount>`. Those are ordinary [`ConfigChange`]s, applied on
//! the running loop like a change from the CLI or the app.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use relay_core::{ConfigChange, SpaceId, validate_name};
use serde::Serialize;

use crate::Engine;
use crate::MaterializationMode;
use crate::error::EngineError;

const SERVER_DATA_KEY: &str = "server_data";

#[derive(Clone, Debug, Serialize)]
pub struct ServerStatus {
    /// Where joined spaces are kept. `None` means this is not a server.
    pub data: Option<PathBuf>,
    pub mounts: Vec<ServerMount>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ServerMount {
    pub space: String,
    pub mount: String,
    /// `None` until the loop attaches it.
    pub path: Option<PathBuf>,
}

impl Engine {
    /// The server data folder, when this device has the server role.
    pub fn server_data(&self) -> Result<Option<PathBuf>, EngineError> {
        Ok(self
            .db
            .repo()
            .local_setting(SERVER_DATA_KEY)?
            .filter(|p| !p.is_empty())
            .map(PathBuf::from))
    }

    /// Give this device the server role, keeping spaces under `path`.
    pub fn set_server_data(&mut self, path: &Path) -> Result<PathBuf, EngineError> {
        self.ensure_writable()?;
        if path.exists() && !std::fs::metadata(path)?.is_dir() {
            return Err(EngineError::PathNotADirectory(path.to_path_buf()));
        }
        std::fs::create_dir_all(path)?;
        let canonical = dunce::canonicalize(path)?;
        let home = dunce::canonicalize(&self.home).unwrap_or_else(|_| self.home.clone());
        if canonical.starts_with(&home) || home.starts_with(&canonical) {
            return Err(EngineError::OverlapsRelayHome);
        }
        self.db
            .transaction(|repo| {
                repo.set_local_setting(SERVER_DATA_KEY, &canonical.to_string_lossy())
            })
            .map_err(EngineError::from_db)?;
        Ok(canonical)
    }

    /// Drop the server role. Joined spaces, mounts, and files stay.
    pub fn clear_server_data(&mut self) -> Result<(), EngineError> {
        self.ensure_writable()?;
        self.db
            .transaction(|repo| repo.clear_local_setting(SERVER_DATA_KEY))
            .map_err(EngineError::from_db)?;
        Ok(())
    }

    pub fn server_status(&self) -> Result<ServerStatus, EngineError> {
        let data = self.server_data()?;
        let mut mounts = Vec::new();
        if data.is_some() {
            for space in self.db.repo().list_spaces()? {
                for config in self.db.repo().list_mounts(Some(space.id))? {
                    mounts.push(ServerMount {
                        space: space.name.clone(),
                        mount: config.mount.name,
                        path: config.local_path,
                    });
                }
            }
        }
        Ok(ServerStatus { data, mounts })
    }

    /// Changes that bring a server up to date with what its peers offer:
    /// join each space offered by a peer that may manage this device, then
    /// attach every mount that has no local folder. Empty unless this device has the server role. Creates the
    /// mount folders, since attaching needs them to exist.
    ///
    /// Offered names come from peers, so a name that is not a single safe
    /// path component is skipped.
    pub fn server_plan(&self) -> Result<Vec<ConfigChange>, EngineError> {
        let Some(data) = self.server_data()? else {
            return Ok(Vec::new());
        };
        let repo = self.db.repo();
        let mut changes = Vec::new();
        let joined: BTreeSet<SpaceId> = repo.list_spaces()?.iter().map(|s| s.id).collect();
        let mut planned: BTreeSet<SpaceId> = BTreeSet::new();
        for offer in repo.list_offers()? {
            if joined.contains(&offer.space_id) || planned.contains(&offer.space_id) {
                continue;
            }
            // Only a peer allowed to manage this device can make it join a
            // space (D37): a manager could ask for the same join remotely
            // (D39), and members adopted from offers (D26) never hold it.
            let may_manage = repo
                .peer_by_id(offer.peer.id)?
                .is_some_and(|peer| peer.may_manage);
            if !may_manage
                || repo.device_status(offer.peer.id)?.as_deref() == Some("revoked")
                || !safe_component(&offer.name)
            {
                continue;
            }
            if repo.space_by_name(&offer.name)?.is_some() {
                // Another space already has this name here; joining would fail.
                continue;
            }
            planned.insert(offer.space_id);
            changes.push(ConfigChange::JoinSpace {
                space: offer.name.clone(),
                from_peer: offer.peer.id.to_string(),
                wait_ms: 0,
            });
            for mount in &offer.mounts {
                changes.extend(attach(&data, &offer.name, &mount.name, false)?);
            }
        }
        for space in repo.list_spaces()? {
            let rules = repo.list_materialization_rules(space.id)?;
            for config in repo.list_mounts(Some(space.id))? {
                if config.local_path.is_some() {
                    continue;
                }
                let root = format!("{}/**", config.mount.name);
                let chosen = rules.iter().any(|rule| rule.selectors.contains(&root));
                changes.extend(attach(&data, &space.name, &config.mount.name, chosen)?);
            }
        }
        Ok(changes)
    }
}

/// Attach `mount` under the data folder. Unless the mount root already has a
/// mode (`chosen`), set it to `store` first, so nothing is written to the
/// folder before the rule exists.
fn attach(
    data: &Path,
    space: &str,
    mount: &str,
    chosen: bool,
) -> Result<Vec<ConfigChange>, EngineError> {
    if !safe_component(space) || !safe_component(mount) {
        return Ok(Vec::new());
    }
    let path = data.join(space).join(mount);
    std::fs::create_dir_all(&path)?;
    let mut changes = Vec::new();
    if !chosen {
        changes.push(ConfigChange::SetFolderMode {
            space: space.to_owned(),
            mount: mount.to_owned(),
            path: String::new(),
            mode: Some(MaterializationMode::Store.as_str().to_owned()),
        });
    }
    changes.push(ConfigChange::AddMount {
        space: space.to_owned(),
        mount: mount.to_owned(),
        path,
        includes: Vec::new(),
        excludes: Vec::new(),
    });
    Ok(changes)
}

/// A valid space or mount name that is also safe as one folder name.
fn safe_component(name: &str) -> bool {
    validate_name(name).is_ok() && name != "." && name != ".." && !name.contains(':')
}

#[cfg(test)]
mod tests {
    use super::safe_component;

    #[test]
    fn components() {
        assert!(safe_component("Projects"));
        assert!(safe_component("my notes"));
        assert!(!safe_component(".."));
        assert!(!safe_component("."));
        assert!(!safe_component("a/b"));
        assert!(!safe_component("C:"));
        assert!(!safe_component(""));
    }
}
