//! Apply a remote index batch (D16–D18).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use relay_core::conflict::{ConflictWinner, choose_group_winner, choose_winner, conflict_path};
use relay_core::{
    DeviceId, EntryContent, EntryKey, EntryKind, EntryRecord, LogicalPath, MOUNT_MARKER, MountId,
    ObjectId, SpaceId, StatHint, TEMP_PREFIX, VersionRelation, compare_versions, is_git_metadata,
};
use relay_db::MountConfig;
use relay_fs::{
    MaterializeOptions, check_real_dir_chain, ensure_real_dir_chain, materialize_file,
    resolve_os_path, to_os_path,
};
use relay_policy::MountRules;
use relay_proto::RemoteEntry;
use relay_store::StoreError;

use crate::Engine;
use crate::error::EngineError;
use crate::order::{apply_sort_key, classify_apply};
use crate::scan::{recorded_stat, wall_clock_now_ns};

const REEVAL_BOUND: u32 = 3;

pub(crate) struct ApplyOutcome {
    pub written: usize,
    pub deleted: usize,
    pub conflicts: usize,
    pub skipped: usize,
    pub warnings: Vec<ApplyWarning>,
    pub transient: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ApplyWarning {
    pub path: String,
    pub reason: String,
}

struct MountApply {
    config: MountConfig,
    rules: MountRules,
    case_insensitive: bool,
    live_fold: HashMap<String, LogicalPath>,
}

impl Engine {
    pub(crate) fn apply_remote_batch(
        &mut self,
        _peer: DeviceId,
        space: SpaceId,
        mut entries: Vec<RemoteEntry>,
        failed_objects: &HashSet<ObjectId>,
    ) -> Result<ApplyOutcome, EngineError> {
        entries.sort_by(|a, b| {
            let local_a = self.db.repo().entry(&a.key).ok().flatten();
            let local_b = self.db.repo().entry(&b.key).ok().flatten();
            let ca = classify_apply(&a.content, local_a.as_ref().and_then(|l| l.content.kind()));
            let cb = classify_apply(&b.content, local_b.as_ref().and_then(|l| l.content.kind()));
            apply_sort_key(&a.key.path, ca).cmp(&apply_sort_key(&b.key.path, cb))
        });

        let mut mounts = self.load_mount_apply(space)?;
        let mut outcome = ApplyOutcome {
            written: 0,
            deleted: 0,
            conflicts: 0,
            skipped: 0,
            warnings: Vec::new(),
            transient: false,
        };

        for entry in entries {
            if let Some(obj) = entry.content.object()
                && failed_objects.contains(&obj)
            {
                outcome.skipped += 1;
                outcome.warnings.push(ApplyWarning {
                    path: entry.key.path.to_string(),
                    reason: format!("object {obj} was not fetched"),
                });
                continue;
            }

            let mount_id = entry.key.mount;
            let Some(ctx) = mounts.get(&mount_id) else {
                outcome.skipped += 1;
                outcome.warnings.push(ApplyWarning {
                    path: entry.key.path.to_string(),
                    reason: "unknown mount".into(),
                });
                continue;
            };
            if let Some(reason) = skip_reason(&entry, ctx) {
                outcome.skipped += 1;
                outcome.warnings.push(ApplyWarning {
                    path: entry.key.path.to_string(),
                    reason,
                });
                continue;
            }

            match self.apply_entry_reeval(entry, &mut mounts) {
                Ok(result) => apply_count(&mut outcome, result),
                Err(err) if is_transient(&err) => {
                    outcome.transient = true;
                    outcome.warnings.push(ApplyWarning {
                        path: String::new(),
                        reason: err.to_string(),
                    });
                    return Ok(outcome);
                }
                Err(err) => {
                    outcome.skipped += 1;
                    outcome.warnings.push(ApplyWarning {
                        path: String::new(),
                        reason: err.to_string(),
                    });
                }
            }
        }
        Ok(outcome)
    }

    fn load_mount_apply(
        &self,
        space: SpaceId,
    ) -> Result<HashMap<MountId, MountApply>, EngineError> {
        let mut out = HashMap::new();
        for config in self.db.repo().list_mounts(Some(space))? {
            let rules = MountRules::new(&config.includes, &config.excludes)?;
            let rules = if let Some(root) = &config.local_path {
                let mut warnings = Vec::new();
                relay_fs::effective_rules(root, &rules, &mut warnings).unwrap_or(rules)
            } else {
                rules
            };
            let case_insensitive = config
                .local_path
                .as_deref()
                .is_some_and(probe_case_insensitive);
            let mut live_fold = HashMap::new();
            for entry in self.db.repo().entries_for_mount(config.mount.id)? {
                if !entry.is_deleted() {
                    live_fold.insert(entry.key.path.case_fold_key(), entry.key.path.clone());
                }
            }
            out.insert(
                config.mount.id,
                MountApply {
                    config,
                    rules,
                    case_insensitive,
                    live_fold,
                },
            );
        }
        Ok(out)
    }

    fn apply_entry_reeval(
        &mut self,
        entry: RemoteEntry,
        mounts: &mut HashMap<MountId, MountApply>,
    ) -> Result<ApplyResult, EngineError> {
        let current = entry;
        for _ in 0..REEVAL_BOUND {
            match self.try_apply_entry(&current, mounts)? {
                TryApply::Done(result) => return Ok(result),
                TryApply::Rescan => {
                    let key = current.key.clone();
                    let space = self
                        .db
                        .repo()
                        .space(key.space)?
                        .ok_or_else(|| EngineError::UnknownSpace(key.space.to_string()))?;
                    let config = self
                        .db
                        .repo()
                        .mount_config(key.mount)?
                        .ok_or(EngineError::MountNotLocal)?;
                    let _ = self.scan_paths(
                        &space.name,
                        &config.mount.name,
                        std::slice::from_ref(&key.path),
                        crate::ScanOptions::default(),
                    )?;
                    if let Some(ctx) = mounts.get_mut(&key.mount) {
                        ctx.live_fold.clear();
                        for rec in self.db.repo().entries_for_mount(key.mount)? {
                            if !rec.is_deleted() {
                                ctx.live_fold
                                    .insert(rec.key.path.case_fold_key(), rec.key.path.clone());
                            }
                        }
                    }
                }
            }
        }
        Ok(ApplyResult::Skipped(
            current.key.path.to_string(),
            "re-evaluate bound exceeded".into(),
        ))
    }

    fn try_apply_entry(
        &mut self,
        remote: &RemoteEntry,
        mounts: &mut HashMap<MountId, MountApply>,
    ) -> Result<TryApply, EngineError> {
        let local = self.db.repo().entry(&remote.key)?;
        let relation = match &local {
            Some(local) => compare_versions(
                &local.vector,
                &local.content,
                &remote.vector,
                &remote.content,
            ),
            None => VersionRelation::RemoteNewer,
        };

        match relation {
            VersionRelation::Same | VersionRelation::LocalNewer => {
                Ok(TryApply::Done(ApplyResult::Nothing))
            }
            VersionRelation::ConcurrentIdentical => {
                self.store_merged_vector(local.as_ref().unwrap(), remote)?;
                Ok(TryApply::Done(ApplyResult::Written))
            }
            VersionRelation::RemoteNewer => self.apply_remote_newer(remote, local.as_ref(), mounts),
            VersionRelation::Conflict | VersionRelation::Diverged => {
                self.apply_conflict(remote, local.as_ref(), mounts)
            }
        }
    }

    fn store_merged_vector(
        &mut self,
        local: &EntryRecord,
        remote: &RemoteEntry,
    ) -> Result<(), EngineError> {
        let mut record = local.clone();
        record.vector = local.vector.merged(&remote.vector);
        self.db.transaction(|repo| {
            let sequence = repo.next_sequence()?;
            record.sequence = sequence;
            repo.put_entry(&record)?;
            Ok::<(), EngineError>(())
        })?;
        Ok(())
    }

    fn apply_remote_newer(
        &mut self,
        remote: &RemoteEntry,
        local: Option<&EntryRecord>,
        mounts: &mut HashMap<MountId, MountApply>,
    ) -> Result<TryApply, EngineError> {
        let ctx = mounts
            .get(&remote.key.mount)
            .ok_or(EngineError::MountNotLocal)?;
        let root = ctx
            .config
            .local_path
            .clone()
            .ok_or(EngineError::MountNotLocal)?;

        match &remote.content {
            EntryContent::File {
                object,
                size,
                executable,
            } => match self.materialize_remote_file(remote, local, &root, *object, *executable)? {
                MaterializeStep::Done(stat) => {
                    self.commit_remote(remote, stat, Some((*object, *size)))?;
                    note_live(mounts, remote);
                    Ok(TryApply::Done(ApplyResult::Written))
                }
                MaterializeStep::Rescan => Ok(TryApply::Rescan),
            },
            EntryContent::Directory => {
                let dest = dest_path(&root, &remote.key.path)?;
                if let Err(err) = ensure_real_dir_chain(&root, &dest) {
                    return skip_or_err(err);
                }
                self.commit_remote(remote, None, None)?;
                note_live(mounts, remote);
                Ok(TryApply::Done(ApplyResult::Written))
            }
            EntryContent::Symlink { target } => {
                if cfg!(windows) {
                    return Ok(TryApply::Done(ApplyResult::Skipped(
                        remote.key.path.to_string(),
                        "symlinks are not materialized on Windows".into(),
                    )));
                }
                let dest = dest_path(&root, &remote.key.path)?;
                if let Some(parent) = dest.parent()
                    && let Err(err) = ensure_real_dir_chain(&root, parent)
                {
                    return skip_or_err(err);
                }
                create_symlink(target, &dest)?;
                let stat = fs::symlink_metadata(&dest)
                    .ok()
                    .map(|m| StatHint::from_metadata(&m));
                self.commit_remote(remote, stat, None)?;
                note_live(mounts, remote);
                Ok(TryApply::Done(ApplyResult::Written))
            }
            EntryContent::Deleted => self.apply_deletion(remote, local, &root, mounts),
        }
    }

    fn apply_deletion(
        &mut self,
        remote: &RemoteEntry,
        local: Option<&EntryRecord>,
        root: &Path,
        mounts: &mut HashMap<MountId, MountApply>,
    ) -> Result<TryApply, EngineError> {
        let dest = match resolve_os_path(root, &remote.key.path)? {
            Some(p) => p,
            None => {
                self.commit_remote(remote, None, None)?;
                note_gone(mounts, remote);
                return Ok(TryApply::Done(ApplyResult::Deleted));
            }
        };
        if let Some(parent) = dest.parent() {
            check_real_dir_chain(root, parent)?;
        }
        let meta = match fs::symlink_metadata(&dest) {
            Ok(m) => m,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.commit_remote(remote, None, None)?;
                note_gone(mounts, remote);
                return Ok(TryApply::Done(ApplyResult::Deleted));
            }
            Err(err) => return Err(EngineError::Io(err)),
        };

        if meta.is_dir() && !meta.file_type().is_symlink() {
            match fs::remove_dir(&dest) {
                Ok(()) => {
                    self.commit_remote(remote, None, None)?;
                    note_gone(mounts, remote);
                    Ok(TryApply::Done(ApplyResult::Deleted))
                }
                Err(_) => Ok(TryApply::Done(ApplyResult::Skipped(
                    remote.key.path.to_string(),
                    "directory is not empty; tombstone not stored".into(),
                ))),
            }
        } else {
            if !file_still_matches(local, &dest, &self.store)? {
                return Ok(TryApply::Rescan);
            }
            fs::remove_file(&dest).map_err(EngineError::Io)?;
            self.commit_remote(remote, None, None)?;
            note_gone(mounts, remote);
            Ok(TryApply::Done(ApplyResult::Deleted))
        }
    }

    fn apply_conflict(
        &mut self,
        remote: &RemoteEntry,
        local: Option<&EntryRecord>,
        mounts: &mut HashMap<MountId, MountApply>,
    ) -> Result<TryApply, EngineError> {
        let Some(local) = local else {
            return self.apply_remote_newer(remote, None, mounts);
        };
        let remote_rec = remote.clone().into_record(local.sequence, None);
        let (winner_remote, copy_loser) = conflict_outcome(local, &remote_rec);

        let ctx = mounts
            .get(&remote.key.mount)
            .ok_or(EngineError::MountNotLocal)?;
        let root = ctx
            .config
            .local_path
            .clone()
            .ok_or(EngineError::MountNotLocal)?;

        let winner = if winner_remote { &remote_rec } else { local };
        let loser = if winner_remote { local } else { &remote_rec };

        let mut copy_path = None;
        if copy_loser && !loser.is_deleted() {
            let counter = loser.vector.get(&loser.modified_by);
            let path = conflict_path(&remote.key.path, &loser.modified_by, counter)
                .map_err(EngineError::InvalidName)?;
            if let Err(step) = self.materialize_conflict_copy(&root, &path, &loser.content)? {
                return Ok(step);
            }
            let copy_key = EntryKey {
                space: remote.key.space,
                mount: remote.key.mount,
                path: path.clone(),
            };
            let copy = EntryRecord {
                key: copy_key,
                content: loser.content.clone(),
                vector: loser.vector.clone(),
                parent_object: loser.parent_object,
                sequence: relay_core::Sequence(0),
                modified_by: loser.modified_by,
                modified_at_unix_ms: loser.modified_at_unix_ms,
                stat: None,
            };
            copy_path = Some((path, copy));
        }

        let mut path_stat = None;
        if winner_remote {
            match &winner.content {
                EntryContent::File {
                    object, executable, ..
                } => match self.materialize_remote_file(
                    remote,
                    Some(local),
                    &root,
                    *object,
                    *executable,
                )? {
                    MaterializeStep::Done(stat) => path_stat = stat,
                    MaterializeStep::Rescan => return Ok(TryApply::Rescan),
                },
                EntryContent::Directory => {
                    let dest = dest_path(&root, &remote.key.path)?;
                    let occupied_by_file = fs::symlink_metadata(&dest)
                        .map(|m| !m.is_dir() || m.file_type().is_symlink())
                        .unwrap_or(false);
                    if occupied_by_file {
                        if !file_still_matches(Some(local), &dest, &self.store)? {
                            return Ok(TryApply::Rescan);
                        }
                        fs::remove_file(&dest).map_err(EngineError::Io)?;
                    }
                    ensure_real_dir_chain(&root, &dest)?;
                }
                other => {
                    if let Err(step) =
                        self.materialize_content(&root, &remote.key.path, other, None)?
                    {
                        return Ok(step);
                    }
                }
            }
        } else {
            path_stat = local.stat;
        }

        let now = self.clock.now_ms();
        let merged = local.vector.merged(&remote.vector);
        let path_key = remote.key.clone();
        let winner_content = winner.content.clone();
        let winner_modified_by = winner.modified_by;
        let winner_parent = winner.parent_object;
        let winner_modified_at = winner.modified_at_unix_ms;
        let copy_path = copy_path;
        self.db.transaction(|repo| {
            if let Some((_, mut copy)) = copy_path.clone() {
                if let Some(obj) = copy.content.object() {
                    let size = match &copy.content {
                        EntryContent::File { size, .. } => *size,
                        _ => 0,
                    };
                    repo.record_object(obj, size, now)?;
                }
                copy.sequence = repo.next_sequence()?;
                repo.put_entry(&copy)?;
            }
            if let Some(obj) = winner_content.object() {
                let size = match &winner_content {
                    EntryContent::File { size, .. } => *size,
                    _ => 0,
                };
                repo.record_object(obj, size, now)?;
            }
            let sequence = repo.next_sequence()?;
            let record = EntryRecord {
                key: path_key,
                content: winner_content,
                vector: merged,
                parent_object: winner_parent,
                sequence,
                modified_by: winner_modified_by,
                modified_at_unix_ms: winner_modified_at,
                stat: path_stat,
            };
            repo.put_entry(&record)?;
            Ok::<(), EngineError>(())
        })?;

        note_live(mounts, remote);
        Ok(TryApply::Done(ApplyResult::Conflict))
    }

    fn materialize_remote_file(
        &mut self,
        remote: &RemoteEntry,
        local: Option<&EntryRecord>,
        root: &Path,
        object: ObjectId,
        executable: bool,
    ) -> Result<MaterializeStep, EngineError> {
        let dest = dest_path(root, &remote.key.path)?;
        let expected = match prepare_expected_existing(&self.store, &dest, local, object)? {
            ExpectedPrep::Matches(stat) => stat,
            ExpectedPrep::Rescan => return Ok(MaterializeStep::Rescan),
        };
        let mut reader = self.store.open_object(&object)?;
        match materialize_file(
            &mut reader,
            &dest,
            object,
            executable,
            expected.as_ref(),
            MaterializeOptions {
                mount_root: root,
                mtime_ns: remote.mtime_ns,
            },
        ) {
            Ok(stat) => Ok(MaterializeStep::Done(racy_stat(stat, &self.config))),
            Err(relay_fs::FsError::DestinationChanged(_)) => Ok(MaterializeStep::Rescan),
            Err(relay_fs::FsError::UnsafeAncestor { path, reason }) => {
                Err(EngineError::Fs(relay_fs::FsError::UnsafeAncestor {
                    path,
                    reason,
                }))
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Write the losing version to its (new) conflict-copy path. An existing
    /// file there with the same bytes is a previous attempt; anything else
    /// means the path is taken and the whole entry is skipped so the local
    /// version is never overwritten without its copy.
    fn materialize_conflict_copy(
        &mut self,
        root: &Path,
        path: &LogicalPath,
        content: &EntryContent,
    ) -> Result<Result<(), TryApply>, EngineError> {
        if let EntryContent::File { object, .. } = content {
            let dest = dest_path(root, path)?;
            if fs::symlink_metadata(&dest).is_ok() {
                return match self.store.hash_file(&dest, None) {
                    Ok(outcome) if outcome.id == *object => Ok(Ok(())),
                    _ => Ok(Err(TryApply::Done(ApplyResult::Skipped(
                        path.to_string(),
                        "conflict copy path is already taken".into(),
                    )))),
                };
            }
        }
        self.materialize_content(root, path, content, None)
    }

    fn materialize_content(
        &mut self,
        root: &Path,
        path: &LogicalPath,
        content: &EntryContent,
        expected_stat: Option<StatHint>,
    ) -> Result<Result<(), TryApply>, EngineError> {
        match content {
            EntryContent::File {
                object, executable, ..
            } => {
                let dest = dest_path(root, path)?;
                let mut reader = self.store.open_object(object)?;
                match materialize_file(
                    &mut reader,
                    &dest,
                    *object,
                    *executable,
                    expected_stat.as_ref(),
                    MaterializeOptions {
                        mount_root: root,
                        mtime_ns: expected_stat.map(|s| s.mtime_ns),
                    },
                ) {
                    Ok(_) => Ok(Ok(())),
                    Err(relay_fs::FsError::DestinationChanged(_)) => Ok(Err(TryApply::Rescan)),
                    Err(err) => Err(err.into()),
                }
            }
            EntryContent::Directory => {
                let dest = dest_path(root, path)?;
                ensure_real_dir_chain(root, &dest)?;
                Ok(Ok(()))
            }
            EntryContent::Symlink { target } => {
                if cfg!(windows) {
                    return Ok(Ok(()));
                }
                let dest = dest_path(root, path)?;
                if let Some(parent) = dest.parent() {
                    ensure_real_dir_chain(root, parent)?;
                }
                create_symlink(target, &dest)?;
                Ok(Ok(()))
            }
            EntryContent::Deleted => Ok(Ok(())),
        }
    }

    fn commit_remote(
        &mut self,
        remote: &RemoteEntry,
        stat: Option<StatHint>,
        object: Option<(ObjectId, u64)>,
    ) -> Result<(), EngineError> {
        let now = self.clock.now_ms();
        let remote = remote.clone();
        self.db.transaction(|repo| {
            if let Some((id, size)) = object {
                repo.record_object(id, size, now)?;
            } else if let Some(id) = remote.content.object() {
                let size = match &remote.content {
                    EntryContent::File { size, .. } => *size,
                    _ => 0,
                };
                repo.record_object(id, size, now)?;
            }
            let sequence = repo.next_sequence()?;
            let record = remote.into_record(sequence, stat);
            repo.put_entry(&record)?;
            Ok::<(), EngineError>(())
        })
    }
}

enum TryApply {
    Done(ApplyResult),
    Rescan,
}

enum ApplyResult {
    Nothing,
    Written,
    Deleted,
    Conflict,
    Skipped(String, String),
}

enum MaterializeStep {
    Done(Option<StatHint>),
    Rescan,
}

enum ExpectedPrep {
    Matches(Option<StatHint>),
    Rescan,
}

fn apply_count(outcome: &mut ApplyOutcome, result: ApplyResult) {
    match result {
        ApplyResult::Nothing => {}
        ApplyResult::Written => outcome.written += 1,
        ApplyResult::Deleted => outcome.deleted += 1,
        ApplyResult::Conflict => outcome.conflicts += 1,
        ApplyResult::Skipped(path, reason) => {
            outcome.skipped += 1;
            outcome.warnings.push(ApplyWarning { path, reason });
        }
    }
}

fn skip_reason(entry: &RemoteEntry, ctx: &MountApply) -> Option<String> {
    if ctx.config.local_path.is_none() {
        return Some("mount is not attached on this device".into());
    }
    if is_reserved(&entry.key.path) {
        return Some("reserved path".into());
    }
    if cfg!(windows) && matches!(entry.content, EntryContent::Symlink { .. }) {
        return Some("symlinks are not supported on Windows".into());
    }
    if !entry.key.path.current_os_issues().is_empty() {
        return Some("name is not representable on this OS".into());
    }
    if to_os_path(ctx.config.local_path.as_ref()?, &entry.key.path).is_err() {
        return Some("name is not representable on this OS".into());
    }
    if let Some(kind) = entry.content.kind()
        && deselected(&ctx.rules, &entry.key.path, kind)
    {
        return Some("excluded by local mount rules".into());
    }
    if ctx.case_insensitive
        && let Some(existing) = ctx.live_fold.get(&entry.key.path.case_fold_key())
        && existing != &entry.key.path
    {
        return Some(format!(
            "case-insensitive collision with live path {existing}"
        ));
    }
    None
}

fn deselected(rules: &MountRules, path: &LogicalPath, kind: EntryKind) -> bool {
    if !rules.is_selected(path, kind) {
        return true;
    }
    if kind == EntryKind::Directory && !rules.should_descend(path) {
        return true;
    }
    let mut current = path.parent();
    while let Some(ancestor) = current {
        if !rules.should_descend(&ancestor) {
            return true;
        }
        current = ancestor.parent();
    }
    false
}

fn is_reserved(path: &LogicalPath) -> bool {
    path.components()
        .any(|c| c == MOUNT_MARKER || c.starts_with(TEMP_PREFIX))
}

fn dest_path(root: &Path, path: &LogicalPath) -> Result<PathBuf, EngineError> {
    match resolve_os_path(root, path)? {
        Some(existing) => Ok(existing),
        None => Ok(to_os_path(root, path)?),
    }
}

fn prepare_expected_existing(
    store: &relay_store::ObjectStore,
    dest: &Path,
    local: Option<&EntryRecord>,
    remote_object: ObjectId,
) -> Result<ExpectedPrep, EngineError> {
    if let Some(record) = local {
        if record.is_deleted() || !matches!(record.content, EntryContent::File { .. }) {
            if dest.exists() {
                return hash_or_rescan(store, dest, None, remote_object);
            }
            return Ok(ExpectedPrep::Matches(None));
        }
        match record.stat {
            Some(stat) => Ok(ExpectedPrep::Matches(Some(stat))),
            None => hash_or_rescan(store, dest, record.content.object(), remote_object),
        }
    } else if dest.exists() {
        hash_or_rescan(store, dest, None, remote_object)
    } else {
        Ok(ExpectedPrep::Matches(None))
    }
}

fn hash_or_rescan(
    store: &relay_store::ObjectStore,
    dest: &Path,
    local_object: Option<ObjectId>,
    remote_object: ObjectId,
) -> Result<ExpectedPrep, EngineError> {
    match store.hash_file(dest, None) {
        Ok(outcome) => {
            if local_object == Some(outcome.id)
                || (local_object.is_none() && outcome.id == remote_object)
            {
                Ok(ExpectedPrep::Matches(Some(outcome.stat)))
            } else {
                Ok(ExpectedPrep::Rescan)
            }
        }
        Err(StoreError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(ExpectedPrep::Matches(None))
        }
        Err(StoreError::SourceChanged { .. }) => Ok(ExpectedPrep::Rescan),
        Err(err) => Err(err.into()),
    }
}

fn file_still_matches(
    local: Option<&EntryRecord>,
    dest: &Path,
    store: &relay_store::ObjectStore,
) -> Result<bool, EngineError> {
    let Some(record) = local else {
        return Ok(true);
    };
    if record.is_deleted() {
        return Ok(true);
    }
    match record.stat {
        Some(stat) => match fs::symlink_metadata(dest) {
            Ok(meta) => Ok(StatHint::from_metadata(&meta) == stat),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(err) => Err(EngineError::Io(err)),
        },
        None => match store.hash_file(dest, None) {
            Ok(outcome) => Ok(Some(outcome.id) == record.content.object()),
            Err(StoreError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                Ok(true)
            }
            Err(err) => Err(err.into()),
        },
    }
}

fn conflict_outcome(local: &EntryRecord, remote: &EntryRecord) -> (bool, bool) {
    let local_tomb = local.is_deleted();
    let remote_tomb = remote.is_deleted();
    if local_tomb ^ remote_tomb {
        return (!remote_tomb, false);
    }
    let local_dir = matches!(local.content, EntryContent::Directory);
    let remote_dir = matches!(remote.content, EntryContent::Directory);
    if local_dir != remote_dir && (local_dir || remote_dir) {
        return (remote_dir, true);
    }
    let choose: fn(&EntryRecord, &EntryRecord) -> ConflictWinner =
        if is_git_metadata(&local.key.path) {
            choose_group_winner
        } else {
            choose_winner
        };
    match choose(local, remote) {
        ConflictWinner::A => (false, true),
        ConflictWinner::B => (true, true),
    }
}

fn create_symlink(target: &str, dest: &Path) -> Result<(), EngineError> {
    #[cfg(unix)]
    {
        if dest.exists() {
            let _ = fs::remove_file(dest);
        }
        std::os::unix::fs::symlink(target, dest).map_err(EngineError::Io)
    }
    #[cfg(not(unix))]
    {
        let _ = (target, dest);
        Ok(())
    }
}

fn racy_stat(stat: StatHint, config: &crate::EngineConfig) -> Option<StatHint> {
    recorded_stat(stat, wall_clock_now_ns(), config.racy_window)
}

fn note_live(mounts: &mut HashMap<MountId, MountApply>, remote: &RemoteEntry) {
    if let Some(ctx) = mounts.get_mut(&remote.key.mount)
        && !remote.content.is_deleted()
    {
        ctx.live_fold
            .insert(remote.key.path.case_fold_key(), remote.key.path.clone());
    }
}

fn note_gone(mounts: &mut HashMap<MountId, MountApply>, remote: &RemoteEntry) {
    if let Some(ctx) = mounts.get_mut(&remote.key.mount) {
        ctx.live_fold.remove(&remote.key.path.case_fold_key());
    }
}

fn skip_or_err<T>(err: relay_fs::FsError) -> Result<T, EngineError> {
    Err(EngineError::Fs(err))
}

fn is_transient(err: &EngineError) -> bool {
    match err {
        EngineError::Io(source) | EngineError::Fs(relay_fs::FsError::Io { source, .. }) => {
            matches!(
                source.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::TimedOut
                    | io::ErrorKind::WouldBlock
            )
        }
        _ => false,
    }
}

pub(crate) fn probe_case_insensitive(root: &Path) -> bool {
    let name = format!("{TEMP_PREFIX}case-probe");
    let path = root.join(&name);
    let variant = root.join(name.to_ascii_uppercase());
    if path == variant {
        return false;
    }
    match fs::write(&path, b"") {
        Ok(()) => {
            let hit = variant.exists();
            let _ = fs::remove_file(&path);
            hit
        }
        Err(_) => false,
    }
}
