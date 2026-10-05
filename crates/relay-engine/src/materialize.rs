//! Local materialization rules (D35).
//!
//! Replication policies still decide who is offered a path. A rule here decides
//! what this device does with a path it wants. Rules are not synced.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use relay_core::{
    EntryContent, EntryKey, LogicalPath, MaterializationRuleId, ObjectId, SpaceId, StatHint,
    validate_name,
};
use relay_db::MaterializationRuleRecord;
use relay_fs::{MaterializeOptions, ensure_real_dir_chain, materialize_file};
use relay_policy::{selector_matches, validate_selector};
use serde::Serialize;

use crate::Engine;
use crate::apply::{create_symlink, dest_path};
use crate::error::EngineError;
use crate::policies::policy_path;
use crate::replica::open_replica;
use crate::scan::{recorded_stat, wall_clock_now_ns};
use crate::secrets::MailboxRead;

/// What this device does with a wanted path. Stored as these strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializationMode {
    Full,
    Metadata,
    Demand,
    Exclude,
    /// Keep every object in the local store, write no working-tree files
    /// (D47). The home server's default.
    Store,
}

impl MaterializationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Metadata => "metadata",
            Self::Demand => "demand",
            Self::Exclude => "exclude",
            Self::Store => "store",
        }
    }

    pub fn parse(value: &str) -> Result<Self, EngineError> {
        match value {
            "full" => Ok(Self::Full),
            "metadata" => Ok(Self::Metadata),
            "demand" => Ok(Self::Demand),
            "exclude" => Ok(Self::Exclude),
            "store" => Ok(Self::Store),
            other => Err(EngineError::UnknownMaterializationMode(other.to_owned())),
        }
    }

    /// Whether a remote file's bytes should be fetched into the store.
    ///
    /// `hydrated` is the local entry's `materialized` flag (`false` when there
    /// is no local row). Demand stays index-only until that flag is set.
    pub fn fetches_bytes(self, hydrated: bool) -> bool {
        match self {
            Self::Full | Self::Store => true,
            Self::Metadata | Self::Exclude => false,
            Self::Demand => hydrated,
        }
    }

    /// Whether a remote file is written into the working tree. A store
    /// path keeps a file already written here (from an earlier `full`)
    /// current, so the folder never holds a stale copy.
    pub fn writes_tree(self, hydrated: bool) -> bool {
        match self {
            Self::Store => hydrated,
            other => other.fetches_bytes(hydrated),
        }
    }
}

/// One local rule, selectors in stored order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MaterializationInfo {
    pub id: MaterializationRuleId,
    pub space: String,
    pub name: String,
    pub mode: String,
    pub position: i64,
    pub selectors: Vec<String>,
}

/// Result of preparing `relay fetch` without opening a peer session.
pub(crate) enum FetchPrep {
    /// Bytes are on disk (or the path has no object) and the index is hydrated.
    Done,
    /// A connected peer may still have the object.
    Need {
        space: SpaceId,
        object: ObjectId,
        key: EntryKey,
    },
}

/// A full-mode entry that still needs a working-tree copy.
pub(crate) struct FullPending {
    pub key: EntryKey,
    pub space: SpaceId,
    pub object: Option<ObjectId>,
}

/// Last matching rule wins. No match is [`MaterializationMode::Full`].
/// `rules` must be ordered by position ascending.
pub(crate) fn path_mode(
    rules: &[MaterializationRuleRecord],
    mount_name: &str,
    relative_path: &str,
) -> Result<MaterializationMode, EngineError> {
    let path = policy_path(mount_name, relative_path);
    let mut mode = MaterializationMode::Full;
    for rule in rules {
        let matched = rule
            .selectors
            .iter()
            .any(|selector| selector_matches(selector, &path).unwrap_or(false));
        if matched {
            mode = MaterializationMode::parse(&rule.mode)?;
        }
    }
    Ok(mode)
}

/// The mode every path of `mount_name` gets, when the rules settle it
/// without looking at paths: no rule reaches inside the mount (`Full`), or
/// the last rule naming `mount/**` is followed by none that could match
/// inside it. `None` means paths may differ.
pub(crate) fn mount_mode(
    rules: &[MaterializationRuleRecord],
    mount_name: &str,
) -> Option<MaterializationMode> {
    let whole = format!("{mount_name}/**");
    let mut mode = Some(MaterializationMode::Full);
    for rule in rules {
        if rule.selectors.contains(&whole) {
            mode = MaterializationMode::parse(&rule.mode).ok();
        } else if rule
            .selectors
            .iter()
            .any(|selector| may_reach(selector, mount_name))
        {
            mode = None;
        }
    }
    mode
}

/// Whether `selector` could match a path in `mount_name`: its first
/// component is the mount or holds a glob character.
fn may_reach(selector: &str, mount_name: &str) -> bool {
    let first = selector.split('/').next().unwrap_or_default();
    first == mount_name || first.contains(['*', '?', '[', '{', '\\'])
}

impl Engine {
    pub fn materialize_add(
        &mut self,
        space: &str,
        name: &str,
        mode: &str,
        selectors: &[String],
    ) -> Result<MaterializationInfo, EngineError> {
        self.ensure_writable()?;
        validate_name(name)?;
        let mode = MaterializationMode::parse(mode)?;
        if selectors.is_empty() {
            return Err(EngineError::EmptyMaterialization);
        }
        for selector in selectors {
            validate_selector(selector)?;
        }
        let space_rec = self
            .db
            .repo()
            .space_by_name(space)?
            .ok_or_else(|| EngineError::UnknownSpace(space.to_owned()))?;
        let id = MaterializationRuleId::new();
        let now = self.clock.now_ms();
        self.db
            .transaction(|repo| {
                repo.create_materialization_rule(
                    id,
                    space_rec.id,
                    name,
                    mode.as_str(),
                    selectors,
                    now,
                )
            })
            .map_err(|err| match err {
                relay_db::DbError::DuplicateName(name) => {
                    EngineError::DuplicateMaterialization(name)
                }
                other => EngineError::from_db(other),
            })?;
        self.materialization_rules(Some(space))?
            .into_iter()
            .find(|rule| rule.name == name)
            .ok_or_else(|| EngineError::UnknownMaterialization(name.to_owned()))
    }

    pub fn materialize_remove(&mut self, space: &str, name: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let space_rec = self
            .db
            .repo()
            .space_by_name(space)?
            .ok_or_else(|| EngineError::UnknownSpace(space.to_owned()))?;
        self.db
            .transaction(|repo| {
                repo.delete_materialization_rule(space_rec.id, name)
                    .map_err(|err| match err {
                        relay_db::DbError::NotFound => {
                            EngineError::UnknownMaterialization(name.to_owned())
                        }
                        other => EngineError::from_db(other),
                    })
            })
            .map_err(|err| match err {
                EngineError::Db(inner) => EngineError::from_db(inner),
                other => other,
            })
    }

    pub fn materialization_rules(
        &self,
        space: Option<&str>,
    ) -> Result<Vec<MaterializationInfo>, EngineError> {
        let spaces = match space {
            Some(name) => {
                let rec = self
                    .db
                    .repo()
                    .space_by_name(name)?
                    .ok_or_else(|| EngineError::UnknownSpace(name.to_owned()))?;
                vec![rec]
            }
            None => self.db.repo().list_spaces()?,
        };
        let mut out = Vec::new();
        for space_rec in spaces {
            for rule in self.db.repo().list_materialization_rules(space_rec.id)? {
                out.push(MaterializationInfo {
                    id: rule.id,
                    space: space_rec.name.clone(),
                    name: rule.name,
                    mode: rule.mode,
                    position: rule.position,
                    selectors: rule.selectors,
                });
            }
        }
        out.sort_by(|a, b| {
            a.space
                .cmp(&b.space)
                .then_with(|| a.position.cmp(&b.position))
        });
        Ok(out)
    }

    /// Hydrate a demand-mode path from the local store or the mailbox.
    ///
    /// Does not open a peer connection. A running host fetches from peers
    /// through [`crate::SyncInput::Fetch`].
    pub fn fetch_path(&mut self, space: &str, mount: &str, path: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        match self.prepare_demand_fetch(space, mount, path)? {
            FetchPrep::Done => Ok(()),
            FetchPrep::Need { .. } => Err(EngineError::ObjectUnavailable),
        }
    }

    /// Drop this device's copy of one demand-mode file, or of every
    /// downloaded demand-mode file under a folder (`""` is the whole mount).
    /// Files whose bytes changed on disk are kept. Returns how many went.
    pub fn evict(&mut self, space: &str, mount: &str, path: &str) -> Result<usize, EngineError> {
        let (_, config) = self.lookup_mount(space, mount)?;
        let folder = if path.is_empty() {
            None
        } else {
            let logical = LogicalPath::new(path)?;
            let key = EntryKey {
                space: config.mount.space,
                mount: config.mount.id,
                path: logical.clone(),
            };
            match self.db.repo().entry(&key)? {
                Some(entry) if !matches!(entry.content, EntryContent::Directory) => {
                    self.evict_path(space, mount, path)?;
                    return Ok(1);
                }
                _ => Some(logical),
            }
        };
        let entries = match &folder {
            Some(prefix) => self.db.repo().entries_under(config.mount.id, prefix)?,
            None => self.db.repo().entries_for_mount(config.mount.id)?,
        };
        let rules = self
            .db
            .repo()
            .list_materialization_rules(config.mount.space)?;
        let mut evicted = 0;
        for entry in entries {
            let is_copy = matches!(
                entry.content,
                EntryContent::File { .. } | EntryContent::Symlink { .. }
            );
            if !is_copy
                || !entry.materialized
                || path_mode(&rules, &config.mount.name, entry.key.path.as_str())?
                    != MaterializationMode::Demand
            {
                continue;
            }
            match self.evict_path(space, mount, entry.key.path.as_str()) {
                Ok(()) => evicted += 1,
                Err(EngineError::EvictMismatch { path }) => {
                    tracing::info!(%path, "kept a changed file while freeing space");
                }
                Err(err) => return Err(err),
            }
        }
        Ok(evicted)
    }

    /// Drop the working-tree copy of a hydrated demand path. The index row stays.
    pub fn evict_path(&mut self, space: &str, mount: &str, path: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let (key, entry, root, label) = self.demand_entry(space, mount, path)?;
        if entry.is_deleted() {
            return Err(EngineError::EntryDeleted(label));
        }
        if !entry.materialized {
            return Err(EngineError::NotMaterialized(label));
        }
        let dest = dest_path(&root, &key.path)?;
        // A placeholder that stays on disk without its bytes (D43).
        let mut placed = None;
        match &entry.content {
            EntryContent::File { object, .. } => {
                let meta = fs::symlink_metadata(&dest).map_err(|err| {
                    if err.kind() == io::ErrorKind::NotFound {
                        EngineError::EvictMismatch {
                            path: label.clone(),
                        }
                    } else {
                        EngineError::Io(err)
                    }
                })?;
                if meta.file_type().is_symlink() || meta.is_dir() {
                    return Err(EngineError::EvictMismatch { path: label });
                }
                if relay_fs::cloud::is_dehydrated(&meta) {
                    // Already freed in Explorer; only the index lagged.
                    if relay_fs::cloud::placeholder_object(&dest) != Some(*object) {
                        return Err(EngineError::EvictMismatch { path: label });
                    }
                    placed = Some(StatHint::from_metadata(&meta));
                } else {
                    let hashed = self.store.hash_file(&dest, None)?;
                    if hashed.id != *object {
                        return Err(EngineError::EvictMismatch { path: label });
                    }
                    let is_placeholder = matches!(
                        relay_fs::cloud::probe(&dest),
                        Ok(relay_fs::cloud::Probe::Placeholder { .. })
                    );
                    if is_placeholder && relay_fs::cloud::dehydrate(&dest).is_ok() {
                        placed = dehydrated_stat(&dest);
                    } else {
                        fs::remove_file(&dest).map_err(EngineError::Io)?;
                    }
                }
            }
            EntryContent::Symlink { target } => {
                let actual = fs::read_link(&dest).map_err(|err| {
                    if err.kind() == io::ErrorKind::NotFound {
                        EngineError::EvictMismatch {
                            path: label.clone(),
                        }
                    } else {
                        EngineError::Io(err)
                    }
                })?;
                let actual = actual.to_str().ok_or_else(|| EngineError::EvictMismatch {
                    path: label.clone(),
                })?;
                if actual != target {
                    return Err(EngineError::EvictMismatch { path: label });
                }
                fs::remove_file(&dest).map_err(EngineError::Io)?;
            }
            EntryContent::Directory => {
                if dest.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
                    let mut children = fs::read_dir(&dest).map_err(EngineError::Io)?;
                    if children
                        .next()
                        .transpose()
                        .map_err(EngineError::Io)?
                        .is_none()
                    {
                        fs::remove_dir(&dest).map_err(EngineError::Io)?;
                    }
                }
            }
            EntryContent::Deleted => return Err(EngineError::EntryDeleted(label)),
        }
        self.db
            .transaction(|repo| repo.set_materialized(&key, false, placed))
            .map_err(EngineError::from_db)?;
        Ok(())
    }

    pub(crate) fn rules_for(
        &self,
        space: SpaceId,
    ) -> Result<Vec<MaterializationRuleRecord>, EngineError> {
        Ok(self.db.repo().list_materialization_rules(space)?)
    }

    /// Whether this remote file's object must be downloaded before apply.
    pub(crate) fn needs_object_bytes(
        &self,
        mode: MaterializationMode,
        key: &EntryKey,
    ) -> Result<bool, EngineError> {
        let hydrated = self
            .db
            .repo()
            .entry(key)?
            .is_some_and(|entry| entry.materialized);
        Ok(mode.fetches_bytes(hydrated))
    }

    pub(crate) fn prepare_demand_fetch(
        &mut self,
        space: &str,
        mount: &str,
        path: &str,
    ) -> Result<FetchPrep, EngineError> {
        let (key, entry, _root, label) = self.demand_entry(space, mount, path)?;
        if entry.is_deleted() {
            return Err(EngineError::EntryDeleted(label));
        }
        match &entry.content {
            EntryContent::File { object, .. } => {
                if !self.store.contains(object)
                    && !self.ingest_mailbox_object(key.space, *object)?
                {
                    return Ok(FetchPrep::Need {
                        space: key.space,
                        object: *object,
                        key,
                    });
                }
                self.materialize_indexed(&key)?;
                Ok(FetchPrep::Done)
            }
            EntryContent::Directory | EntryContent::Symlink { .. } => {
                self.materialize_indexed(&key)?;
                Ok(FetchPrep::Done)
            }
            EntryContent::Deleted => Err(EngineError::EntryDeleted(label)),
        }
    }

    /// Write one indexed entry from bytes already in the store and mark it hydrated.
    pub(crate) fn materialize_indexed(&mut self, key: &EntryKey) -> Result<(), EngineError> {
        let entry = self
            .db
            .repo()
            .entry(key)?
            .ok_or_else(|| EngineError::UnknownEntry(key.path.to_string()))?;
        if entry.is_deleted() {
            return Err(EngineError::EntryDeleted(key.path.to_string()));
        }
        let config = self
            .db
            .repo()
            .mount_config(key.mount)?
            .ok_or(EngineError::MountNotLocal)?;
        let root = config.local_path.ok_or(EngineError::MountNotLocal)?;
        let dest = dest_path(&root, &key.path)?;
        let stat = match &entry.content {
            EntryContent::File {
                object, executable, ..
            } => {
                if !self.store.contains(object) {
                    return Err(EngineError::ObjectUnavailable);
                }
                self.write_stored_file(&root, &dest, *object, *executable)?
            }
            EntryContent::Directory => {
                if dest
                    .symlink_metadata()
                    .is_ok_and(|meta| !meta.is_dir() || meta.file_type().is_symlink())
                {
                    return Err(EngineError::EvictMismatch {
                        path: key.path.to_string(),
                    });
                }
                ensure_real_dir_chain(&root, &dest)?;
                observed_stat(&dest, &self.config)
            }
            EntryContent::Symlink { target } => {
                if cfg!(windows) {
                    return Err(EngineError::ObjectUnavailable);
                }
                if let Some(parent) = dest.parent() {
                    ensure_real_dir_chain(&root, parent)?;
                }
                let existing = fs::read_link(&dest)
                    .ok()
                    .and_then(|p| p.into_os_string().into_string().ok());
                if existing.as_deref() != Some(target.as_str()) {
                    create_symlink(target, &dest)?;
                }
                observed_stat(&dest, &self.config)
            }
            EntryContent::Deleted => return Ok(()),
        };
        let object = entry.content.object();
        let size = match &entry.content {
            EntryContent::File { size, .. } => *size,
            _ => 0,
        };
        let now = self.clock.now_ms();
        self.db
            .transaction(|repo| {
                if let Some(object) = object {
                    repo.record_object(object, size, now)?;
                }
                repo.set_materialized(key, true, stat)?;
                Ok::<(), relay_db::DbError>(())
            })
            .map_err(EngineError::from_db)?;
        Ok(())
    }

    /// Full-mode entries that are still index-only.
    ///
    /// Skips mounts whose rules give every path another mode, so a server
    /// whose mounts are all `store` reads no rows here.
    pub(crate) fn full_unmaterialized(&self) -> Result<Vec<FullPending>, EngineError> {
        self.index_only_in_mode(MaterializationMode::Full)
    }

    /// Store-mode file rows whose object is not in the local store.
    pub(crate) fn store_unfetched(&self) -> Result<Vec<FullPending>, EngineError> {
        let mut out = self.index_only_in_mode(MaterializationMode::Store)?;
        out.retain(|item| {
            item.object
                .is_some_and(|object| !self.store.contains(&object))
        });
        Ok(out)
    }

    /// Index-only rows of attached mounts whose path is in `mode`.
    fn index_only_in_mode(
        &self,
        mode: MaterializationMode,
    ) -> Result<Vec<FullPending>, EngineError> {
        let repo = self.db.repo();
        let mut out = Vec::new();
        for space in repo.list_spaces()? {
            let rules = repo.list_materialization_rules(space.id)?;
            for config in repo.list_mounts(Some(space.id))? {
                if config.local_path.is_none() {
                    continue;
                }
                let uniform = mount_mode(&rules, &config.mount.name);
                if uniform.is_some_and(|m| m != mode) {
                    continue;
                }
                for row in repo.list_index_only_in(config.mount.id)? {
                    if uniform.is_none()
                        && path_mode(&rules, &row.mount_name, row.path.as_str())? != mode
                    {
                        continue;
                    }
                    out.push(FullPending {
                        object: row.object,
                        key: EntryKey {
                            space: row.space,
                            mount: row.mount,
                            path: row.path,
                        },
                        space: row.space,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Note that `object` is now in the store, so store-mode rows naming it
    /// stop counting as unfetched.
    pub(crate) fn record_stored_object(&mut self, object: ObjectId) -> Result<(), EngineError> {
        if !self.store.contains(&object) {
            return Ok(());
        }
        let size = self.store.size_of(&object)?;
        let now = self.clock.now_ms();
        self.db
            .transaction(|repo| repo.record_object(object, size, now))
            .map_err(EngineError::from_db)
    }

    /// Materialize full-mode index rows that were waiting on `object`.
    ///
    /// Reads only the index-only rows that name this object, so a fetch
    /// does not cost a walk of every index-only row.
    pub(crate) fn materialize_full_object(&mut self, object: ObjectId) -> Result<(), EngineError> {
        use std::collections::HashMap;

        if !self.store.contains(&object) {
            return Ok(());
        }
        let rows = self.db.repo().list_index_only_by_object(object)?;
        let mut rules_by_space: HashMap<SpaceId, Vec<MaterializationRuleRecord>> = HashMap::new();
        for row in rows {
            let rules = match rules_by_space.get(&row.space) {
                Some(rules) => rules,
                None => {
                    let loaded = self.db.repo().list_materialization_rules(row.space)?;
                    rules_by_space.insert(row.space, loaded);
                    rules_by_space.get(&row.space).expect("just inserted")
                }
            };
            if path_mode(rules, &row.mount_name, row.path.as_str())? != MaterializationMode::Full {
                continue;
            }
            let key = EntryKey {
                space: row.space,
                mount: row.mount,
                path: row.path,
            };
            match self.materialize_indexed(&key) {
                Ok(()) => {}
                // An unscanned file at that path: the scan reconciles it;
                // the other rows waiting on this object still write.
                Err(EngineError::DestinationChanged(path)) => {
                    tracing::info!(path = %path.display(), "kept an unscanned file during hydration");
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    /// Copy a mailbox object into the local store. `false` when it is absent.
    pub(crate) fn ingest_mailbox_object(
        &mut self,
        space: SpaceId,
        object: ObjectId,
    ) -> Result<bool, EngineError> {
        if self.store.contains(&object) {
            return Ok(true);
        }
        let Some(path) = self.replica_path()? else {
            return Ok(false);
        };
        let replica = open_replica(&path)?;
        match self.take_space_object(&replica, space, object)? {
            MailboxRead::Ready(bytes) if ObjectId::of(&bytes) == object => {
                self.store.put_bytes(&bytes)?;
                Ok(true)
            }
            MailboxRead::Ready(_) | MailboxRead::Locked | MailboxRead::Missing => Ok(false),
        }
    }

    fn demand_entry(
        &self,
        space: &str,
        mount: &str,
        path: &str,
    ) -> Result<(EntryKey, relay_core::EntryRecord, PathBuf, String), EngineError> {
        let logical = LogicalPath::new(path)?;
        let (space_rec, config) = self.lookup_mount(space, mount)?;
        let root = config
            .local_path
            .clone()
            .ok_or(EngineError::MountNotLocal)?;
        let key = EntryKey {
            space: space_rec.id,
            mount: config.mount.id,
            path: logical,
        };
        let label = format!("{space}/{mount}/{path}");
        let entry = self
            .db
            .repo()
            .entry(&key)?
            .ok_or_else(|| EngineError::UnknownEntry(label.clone()))?;
        let rules = self.db.repo().list_materialization_rules(space_rec.id)?;
        let mode = path_mode(&rules, &config.mount.name, key.path.as_str())?;
        if mode != MaterializationMode::Demand {
            return Err(EngineError::NotDemand {
                path: label,
                mode: mode.as_str().to_owned(),
            });
        }
        Ok((key, entry, root, label))
    }

    fn write_stored_file(
        &self,
        root: &Path,
        dest: &Path,
        object: ObjectId,
        executable: bool,
    ) -> Result<Option<StatHint>, EngineError> {
        if let Ok(meta) = fs::symlink_metadata(dest)
            && meta.is_file()
            && !meta.file_type().is_symlink()
        {
            if relay_fs::cloud::is_dehydrated(&meta) {
                // A placeholder without data: replace it, never read it.
                let stat = StatHint::from_metadata(&meta);
                return self.materialize_over(root, dest, object, executable, Some(stat));
            }
            match self.store.hash_file(dest, None) {
                Ok(outcome) if outcome.id == object => {
                    return Ok(observed_stat(dest, &self.config));
                }
                // Other bytes under an index-only row are an edit the scanner
                // has not seen; overwriting them would lose it. The scan
                // records the edit, after which no hydration is needed.
                Ok(_) => return Err(EngineError::DestinationChanged(dest.to_path_buf())),
                Err(err) => return Err(err.into()),
            }
        }
        if fs::symlink_metadata(dest).is_ok() {
            return Err(EngineError::DestinationChanged(dest.to_path_buf()));
        }
        self.materialize_over(root, dest, object, executable, None)
    }

    fn materialize_over(
        &self,
        root: &Path,
        dest: &Path,
        object: ObjectId,
        executable: bool,
        expected: Option<StatHint>,
    ) -> Result<Option<StatHint>, EngineError> {
        let mut reader = self.store.open_object(&object)?;
        let stat = materialize_file(
            &mut reader,
            dest,
            object,
            executable,
            expected.as_ref(),
            MaterializeOptions {
                mount_root: root,
                mtime_ns: None,
            },
        )?;
        Ok(recorded_stat(
            stat,
            wall_clock_now_ns(),
            self.config.racy_window,
        ))
    }
}

/// Stat of a Cloud Files placeholder without data at `path` (D43). This
/// process must never read such a file: the read would wait on a download
/// this process serves.
pub(crate) fn dehydrated_stat(path: &Path) -> Option<StatHint> {
    let meta = fs::symlink_metadata(path).ok()?;
    (meta.is_file() && relay_fs::cloud::is_dehydrated(&meta))
        .then(|| StatHint::from_metadata(&meta))
}

fn observed_stat(dest: &Path, config: &crate::EngineConfig) -> Option<StatHint> {
    let meta = fs::symlink_metadata(dest).ok()?;
    recorded_stat(
        StatHint::from_metadata(&meta),
        wall_clock_now_ns(),
        config.racy_window,
    )
}

#[cfg(test)]
mod tests {
    use super::{MaterializationMode, mount_mode};
    use relay_core::{MaterializationRuleId, SpaceId};
    use relay_db::MaterializationRuleRecord;

    fn rule(mode: &str, selectors: &[&str]) -> MaterializationRuleRecord {
        MaterializationRuleRecord {
            id: MaterializationRuleId::new(),
            space_id: SpaceId::new(),
            name: "r".into(),
            mode: mode.into(),
            position: 0,
            selectors: selectors.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn mount_mode_is_known_only_when_no_rule_reaches_inside() {
        use MaterializationMode::{Full, Store};
        assert_eq!(mount_mode(&[], "files"), Some(Full));
        let store = rule("store", &["files/**"]);
        assert_eq!(
            mount_mode(std::slice::from_ref(&store), "files"),
            Some(Store)
        );
        assert_eq!(
            mount_mode(std::slice::from_ref(&store), "other"),
            Some(Full)
        );
        let sub = rule("full", &["files/raw/**"]);
        assert_eq!(mount_mode(&[store.clone(), sub.clone()], "files"), None);
        assert_eq!(mount_mode(&[sub, store.clone()], "files"), Some(Store));
        let glob = rule("metadata", &["**/*.iso"]);
        assert_eq!(mount_mode(&[store, glob], "files"), None);
    }
}
