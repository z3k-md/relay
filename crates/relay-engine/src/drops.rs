//! Drop settings (D50): whether this device takes files that other devices
//! push to it, and the folder they land in.
//!
//! Both are device-local settings. The daemon reads them for every drop, so
//! a change applies to the next drop without reloading the host.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::Engine;
use crate::error::EngineError;

const DROPS_RECEIVE_KEY: &str = "drops_receive";
const DROP_INBOX_KEY: &str = "drop_inbox";
/// Folder name under Pictures for the default inbox.
const DEFAULT_INBOX_NAME: &str = "Relay";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DropSettings {
    /// Whether trusted peers may push files here. On by default, except on a
    /// device with the server role (D45).
    pub receive: bool,
    /// Where drops land. `None` only when no default can be found and none
    /// was set.
    pub inbox: Option<PathBuf>,
    /// The inbox was chosen rather than defaulted.
    pub inbox_set: bool,
}

impl Engine {
    pub fn drop_settings(&self) -> Result<DropSettings, EngineError> {
        let repo = self.db.repo();
        let receive = match repo.local_setting(DROPS_RECEIVE_KEY)?.as_deref() {
            Some("1") => true,
            Some("0") => false,
            _ => self.server_data()?.is_none(),
        };
        let set = repo
            .local_setting(DROP_INBOX_KEY)?
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        Ok(DropSettings {
            receive,
            inbox_set: set.is_some(),
            inbox: set.or_else(default_drop_inbox),
        })
    }

    /// Take drops from trusted peers, or refuse them.
    pub fn set_drops_receive(&mut self, receive: bool) -> Result<DropSettings, EngineError> {
        self.ensure_writable()?;
        let value = if receive { "1" } else { "0" };
        self.db
            .transaction(|repo| repo.set_local_setting(DROPS_RECEIVE_KEY, value))
            .map_err(EngineError::from_db)?;
        self.drop_settings()
    }

    /// Put drops in `path`, created if missing, or back in the default
    /// folder when `None`. The inbox may not be inside the Relay home or
    /// hold it.
    pub fn set_drop_inbox(&mut self, path: Option<&Path>) -> Result<DropSettings, EngineError> {
        self.ensure_writable()?;
        let Some(path) = path else {
            self.db
                .transaction(|repo| repo.clear_local_setting(DROP_INBOX_KEY))
                .map_err(EngineError::from_db)?;
            return self.drop_settings();
        };
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
                repo.set_local_setting(DROP_INBOX_KEY, &canonical.to_string_lossy())
            })
            .map_err(EngineError::from_db)?;
        self.drop_settings()
    }
}

/// `Pictures/Relay` in the user's home, where the platform has a Pictures
/// folder.
pub fn default_drop_inbox() -> Option<PathBuf> {
    let dirs = directories::UserDirs::new()?;
    let pictures = dirs
        .picture_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dirs.home_dir().join("Pictures"));
    Some(pictures.join(DEFAULT_INBOX_NAME))
}
