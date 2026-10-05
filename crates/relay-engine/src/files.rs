//! What a Files view shows for one folder of a mount, and the per-folder
//! "keep on this device" / "online only" choice (D35, remote explorer
//! Stage 1).
//!
//! A folder choice is a materialization rule named `folder-…` whose
//! selectors are `mount/path/**` and the folder itself, `mount/path` (so an
//! excluded folder does not arrive as an empty one). Choosing for a folder replaces the folder rules
//! inside it, so the newest choice for a parent covers everything in it, the
//! way "apply to enclosed items" works in a file manager. Rules a person wrote
//! with `relay materialize` are never touched.

use std::path::PathBuf;

use relay_core::conflict::is_conflict_copy;
use relay_core::{EntryContent, EntryKind, LogicalPath, MaterializationRuleId, validate_name};
use relay_fs::resolve_os_path;
use serde::Serialize;

use crate::Engine;
use crate::error::EngineError;
use crate::materialize::{MaterializationMode, path_mode};
use crate::policies::policy_path;

/// Name prefix of rules the Files view owns.
const FOLDER_RULE_PREFIX: &str = "folder-";

/// Whether this device holds a file's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyState {
    Local,
    /// Demand mode, not downloaded. Opening or downloading fetches it.
    OnlineOnly,
    /// Metadata mode: this device keeps only the index row.
    MetadataOnly,
    /// Full mode, not written yet: still syncing, or no device had it.
    Pending,
    /// Store mode: the bytes stay in this device's object store, with no
    /// working-tree file (D47).
    Stored,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileRow {
    pub name: String,
    /// Path inside the mount, `/`-separated.
    pub path: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub modified_ms: i64,
    pub state: CopyState,
    pub mode: MaterializationMode,
    pub conflict_copy: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct FolderView {
    pub space: String,
    pub mount: String,
    /// The mount's folder on this device.
    pub root: Option<PathBuf>,
    /// This folder inside the mount; `""` is the mount itself.
    pub path: String,
    /// What a new file here gets.
    pub mode: MaterializationMode,
    /// A Files-view choice made for exactly this folder, if any.
    pub chosen_here: Option<MaterializationMode>,
    /// Folders first, then by name ignoring case.
    pub entries: Vec<FileRow>,
}

impl Engine {
    pub fn list_folder(
        &self,
        space: &str,
        mount: &str,
        path: &str,
    ) -> Result<FolderView, EngineError> {
        let (space_rec, config) = self.lookup_mount(space, mount)?;
        let folder = folder_path(path)?;
        let rules = self.db.repo().list_materialization_rules(space_rec.id)?;
        let name = &config.mount.name;
        let mut entries = Vec::new();
        for entry in self
            .db
            .repo()
            .entries_in(config.mount.id, folder.as_ref())?
        {
            let Some(kind) = entry.content.kind() else {
                continue;
            };
            let mode = path_mode(&rules, name, entry.key.path.as_str())?;
            entries.push(FileRow {
                name: entry.key.path.file_name().to_owned(),
                path: entry.key.path.as_str().to_owned(),
                kind,
                size: match entry.content {
                    EntryContent::File { size, .. } => Some(size),
                    _ => None,
                },
                modified_ms: entry.modified_at_unix_ms,
                state: copy_state(entry.materialized, mode),
                mode,
                conflict_copy: is_conflict_copy(&entry.key.path),
            });
        }
        entries.sort_by(|a, b| {
            (b.kind == EntryKind::Directory)
                .cmp(&(a.kind == EntryKind::Directory))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        let selector = folder_selector(name, path);
        let chosen_here = rules
            .iter()
            .rfind(|rule| is_folder_rule(&rule.name, &rule.selectors, &selector))
            .map(|rule| MaterializationMode::parse(&rule.mode))
            .transpose()?;
        Ok(FolderView {
            space: space_rec.name,
            mount: name.clone(),
            root: config.local_path,
            // A new file in this folder; a rule for `folder/**` matches it.
            mode: path_mode(&rules, name, &child_probe(path))?,
            chosen_here,
            path: path.to_owned(),
            entries,
        })
    }

    /// Choose what this device keeps for everything in a folder (`""` is the
    /// whole mount). `None` drops the choice so the enclosing folder's
    /// applies. Files already here stay until they are freed (D35).
    ///
    /// Needs only the space: rules name mounts by name, so a choice can be
    /// made before the mount exists, and so before its first scan.
    pub fn set_folder_mode(
        &mut self,
        space: &str,
        mount: &str,
        path: &str,
        mode: Option<MaterializationMode>,
    ) -> Result<(), EngineError> {
        self.ensure_writable()?;
        validate_name(mount)?;
        let space_rec = self
            .db
            .repo()
            .space_by_name(space)?
            .ok_or_else(|| EngineError::UnknownSpace(space.to_owned()))?;
        folder_path(path)?;
        let selector = folder_selector(mount, path);
        let inside = selector.trim_end_matches("**").to_owned();
        let replaced: Vec<String> = self
            .db
            .repo()
            .list_materialization_rules(space_rec.id)?
            .into_iter()
            .filter(|rule| {
                rule.name.starts_with(FOLDER_RULE_PREFIX)
                    && rule
                        .selectors
                        .first()
                        .is_some_and(|first| first.starts_with(&inside))
            })
            .map(|rule| rule.name)
            .collect();
        let now = self.clock.now_ms();
        self.db
            .transaction(|repo| {
                for name in &replaced {
                    repo.delete_materialization_rule(space_rec.id, name)?;
                }
                if let Some(mode) = mode {
                    let id = MaterializationRuleId::new();
                    let name = format!("{FOLDER_RULE_PREFIX}{}", &id.to_string()[..8]);
                    repo.create_materialization_rule(
                        id,
                        space_rec.id,
                        &name,
                        mode.as_str(),
                        &folder_selectors(mount, path),
                        now,
                    )?;
                }
                Ok::<(), relay_db::DbError>(())
            })
            .map_err(EngineError::from_db)
    }
}

impl Engine {
    /// Where a file of a mount lives on this device. Refuses paths that would
    /// leave the mount through a symlink.
    pub fn local_file_path(
        &self,
        space: &str,
        mount: &str,
        path: &str,
    ) -> Result<PathBuf, EngineError> {
        let (_, config) = self.lookup_mount(space, mount)?;
        let root = config.local_path.ok_or(EngineError::MountNotLocal)?;
        let logical = LogicalPath::new(path)?;
        resolve_os_path(&root, &logical)?
            .ok_or_else(|| EngineError::UnknownEntry(format!("{space}/{mount}/{path}")))
    }
}

fn copy_state(materialized: bool, mode: MaterializationMode) -> CopyState {
    match (materialized, mode) {
        (true, _) => CopyState::Local,
        (false, MaterializationMode::Demand) => CopyState::OnlineOnly,
        (false, MaterializationMode::Metadata | MaterializationMode::Exclude) => {
            CopyState::MetadataOnly
        }
        (false, MaterializationMode::Full) => CopyState::Pending,
        (false, MaterializationMode::Store) => CopyState::Stored,
    }
}

/// `""` is the mount root; anything else must be a valid logical path.
fn folder_path(path: &str) -> Result<Option<LogicalPath>, EngineError> {
    if path.is_empty() {
        Ok(None)
    } else {
        Ok(Some(LogicalPath::new(path)?))
    }
}

/// `mount/path/**`, or `mount/**` for the whole mount.
fn folder_selector(mount: &str, path: &str) -> String {
    format!("{}/**", policy_path(mount, path))
}

/// `mount/path/**` then the folder itself; just `mount/**` for the mount.
fn folder_selectors(mount: &str, path: &str) -> Vec<String> {
    let mut selectors = vec![folder_selector(mount, path)];
    if !path.is_empty() {
        selectors.push(policy_path(mount, path));
    }
    selectors
}

/// A file directly inside `path`, for asking what mode it would get.
fn child_probe(path: &str) -> String {
    if path.is_empty() {
        "file".to_owned()
    } else {
        format!("{path}/file")
    }
}

fn is_folder_rule(name: &str, selectors: &[String], selector: &str) -> bool {
    name.starts_with(FOLDER_RULE_PREFIX) && selectors.first().is_some_and(|s| s == selector)
}
