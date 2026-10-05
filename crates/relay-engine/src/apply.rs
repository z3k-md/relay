//! Apply a remote index batch (D16–D18).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use relay_core::conflict::{ConflictWinner, choose_group_winner, choose_winner, conflict_path};
use relay_core::{
    DeviceId, EntryContent, EntryKey, EntryKind, EntryRecord, LogicalPath, MergeOutcome, MountId,
    ObjectId, SpaceId, StatHint, TEMP_PREFIX, VectorOrdering, VersionRelation, compare_versions,
    is_bookkeeping_path, is_git_metadata, merge_text,
};
use relay_db::{MaterializationRuleRecord, MountConfig};
use relay_fs::{
    MaterializeOptions, check_real_dir_chain, ensure_real_dir_chain, materialize_file,
    resolve_os_path, to_os_path,
};
use relay_policy::MountRules;
use relay_proto::RemoteEntry;
use relay_store::StoreError;

use crate::Engine;
use crate::error::EngineError;
use crate::materialize::{dehydrated_stat, path_mode};
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
    /// Sender sequence of the entry a transient error stopped the batch at.
    /// It and everything after it in the batch were not applied.
    pub stalled_at: Option<u64>,
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
    /// Case-folded live paths, built on first use and only for a mount whose
    /// filesystem folds case; the collision check is the only reader.
    live_fold: Option<HashMap<String, LogicalPath>>,
}

impl Engine {
    pub(crate) fn apply_remote_batch(
        &mut self,
        _peer: DeviceId,
        space: SpaceId,
        mut entries: Vec<RemoteEntry>,
        failed_objects: &HashSet<ObjectId>,
    ) -> Result<ApplyOutcome, EngineError> {
        // One index read per entry, shared by the sort and the loop below.
        let mut locals: HashMap<EntryKey, Option<EntryRecord>> =
            HashMap::with_capacity(entries.len());
        for entry in &entries {
            if !locals.contains_key(&entry.key) {
                locals.insert(entry.key.clone(), self.db.repo().entry(&entry.key)?);
            }
        }
        let class_of = |entry: &RemoteEntry| {
            let previous = locals
                .get(&entry.key)
                .and_then(|local| local.as_ref())
                .and_then(|local| local.content.kind());
            classify_apply(&entry.content, previous)
        };
        entries.sort_by(|a, b| {
            apply_sort_key(&a.key.path, class_of(a)).cmp(&apply_sort_key(&b.key.path, class_of(b)))
        });

        let mut mounts = self.load_mount_apply(space)?;
        let rules = self.db.repo().list_materialization_rules(space)?;
        let mut outcome = ApplyOutcome {
            written: 0,
            deleted: 0,
            conflicts: 0,
            skipped: 0,
            warnings: Vec::new(),
            transient: false,
            stalled_at: None,
        };

        for entry in entries {
            let sequence = entry.sequence.0;
            let local = locals.get(&entry.key).and_then(|local| local.as_ref());
            let writing = self.writing_remote(&rules, &entry, local).unwrap_or(true);
            if writing
                && let Some(obj) = entry.content.object()
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
            if let Some(ctx) = mounts.get_mut(&mount_id)
                && ctx.case_insensitive
                && ctx.live_fold.is_none()
            {
                ctx.live_fold = Some(self.live_fold_for(mount_id)?);
            }
            let Some(ctx) = mounts.get(&mount_id) else {
                outcome.skipped += 1;
                outcome.warnings.push(ApplyWarning {
                    path: entry.key.path.to_string(),
                    reason: "unknown mount".into(),
                });
                continue;
            };
            if let Some(reason) = skip_reason(&entry, ctx, writing) {
                outcome.skipped += 1;
                outcome.warnings.push(ApplyWarning {
                    path: entry.key.path.to_string(),
                    reason,
                });
                continue;
            }

            match self.apply_entry_reeval(entry, &mut mounts, &rules) {
                Ok(result) => apply_count(&mut outcome, result),
                Err(err) if is_transient(&err) => {
                    outcome.transient = true;
                    outcome.stalled_at = Some(sequence);
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
        &mut self,
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
            let case_insensitive = match &config.local_path {
                Some(root) => self.case_insensitive_root(config.mount.id, root),
                None => false,
            };
            out.insert(
                config.mount.id,
                MountApply {
                    config,
                    rules,
                    case_insensitive,
                    live_fold: None,
                },
            );
        }
        Ok(out)
    }

    /// Whether `root` folds case. Probed once per mount path: the probe
    /// writes and removes a file in the root, which is not a per-batch cost.
    fn case_insensitive_root(&mut self, mount: MountId, root: &Path) -> bool {
        match self.case_probe.get(&mount) {
            Some((path, answer)) if path == root => *answer,
            _ => {
                let answer = probe_case_insensitive(root);
                self.case_probe.insert(mount, (root.to_path_buf(), answer));
                answer
            }
        }
    }

    fn live_fold_for(&self, mount: MountId) -> Result<HashMap<String, LogicalPath>, EngineError> {
        let paths = self.db.repo().live_paths(mount)?;
        let mut fold = HashMap::with_capacity(paths.len());
        for path in paths {
            fold.insert(path.case_fold_key(), path);
        }
        Ok(fold)
    }

    fn apply_entry_reeval(
        &mut self,
        entry: RemoteEntry,
        mounts: &mut HashMap<MountId, MountApply>,
        rules: &[MaterializationRuleRecord],
    ) -> Result<ApplyResult, EngineError> {
        let current = entry;
        for _ in 0..REEVAL_BOUND {
            match self.try_apply_entry(&current, mounts, rules)? {
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
                        // Rebuilt on the next entry of this mount, if needed.
                        ctx.live_fold = None;
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
        rules: &[MaterializationRuleRecord],
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
        let writing = self.writing_remote(rules, remote, local.as_ref())?;

        match relation {
            VersionRelation::Same | VersionRelation::LocalNewer => {
                Ok(TryApply::Done(ApplyResult::Nothing))
            }
            VersionRelation::ConcurrentIdentical => {
                self.store_merged_vector(local.as_ref().unwrap(), remote)?;
                Ok(TryApply::Done(ApplyResult::Written))
            }
            VersionRelation::RemoteNewer if !writing => self.commit_index_only(remote, mounts),
            VersionRelation::RemoteNewer => self.apply_remote_newer(remote, local.as_ref(), mounts),
            VersionRelation::Conflict | VersionRelation::Diverged if !writing => {
                self.commit_unmaterialized_conflict(remote, local.as_ref(), mounts)?;
                Ok(TryApply::Done(ApplyResult::Conflict))
            }
            VersionRelation::Conflict | VersionRelation::Diverged => {
                self.apply_conflict(remote, local.as_ref(), mounts)
            }
        }
    }

    /// Whether this device should write `remote` into the working tree.
    fn writing_remote(
        &self,
        rules: &[MaterializationRuleRecord],
        remote: &RemoteEntry,
        local: Option<&EntryRecord>,
    ) -> Result<bool, EngineError> {
        let Some(config) = self.db.repo().mount_config(remote.key.mount)? else {
            return Ok(true);
        };
        let mode = path_mode(rules, &config.mount.name, remote.key.path.as_str())?;
        let hydrated = local.is_some_and(|entry| entry.materialized);
        Ok(mode.fetches_bytes(hydrated))
    }

    fn commit_index_only(
        &mut self,
        remote: &RemoteEntry,
        mounts: &mut HashMap<MountId, MountApply>,
    ) -> Result<TryApply, EngineError> {
        self.commit_remote(remote, None, None, false)?;
        if remote.content.is_deleted() {
            note_gone(mounts, remote);
            Ok(TryApply::Done(ApplyResult::Deleted))
        } else {
            note_live(mounts, remote);
            Ok(TryApply::Done(ApplyResult::Written))
        }
    }

    /// Concurrent edit on a path this device is not writing. Resolve it in the
    /// index exactly as a writing device would (D18): the winner's content
    /// under the merged vector, the loser as an index-only row at its
    /// conflict-copy path. Nothing is written to the working tree, and every
    /// device still reaches the same rows.
    fn commit_unmaterialized_conflict(
        &mut self,
        remote: &RemoteEntry,
        local: Option<&EntryRecord>,
        mounts: &mut HashMap<MountId, MountApply>,
    ) -> Result<(), EngineError> {
        let Some(local) = local else {
            self.commit_remote(remote, None, None, false)?;
            note_live(mounts, remote);
            return Ok(());
        };
        let remote_rec = remote.clone().into_record(local.sequence, None, false);
        let (winner_remote, copy_loser) = conflict_outcome(local, &remote_rec);
        let winner = if winner_remote { &remote_rec } else { local };
        let loser = if winner_remote { local } else { &remote_rec };

        let mut copy = None;
        if copy_loser && !loser.is_deleted() {
            let counter = loser.vector.get(&loser.modified_by);
            let path = conflict_path(&remote.key.path, &loser.modified_by, counter)
                .map_err(EngineError::InvalidName)?;
            copy = Some(EntryRecord {
                key: EntryKey {
                    space: remote.key.space,
                    mount: remote.key.mount,
                    path,
                },
                content: loser.content.clone(),
                vector: loser.vector.clone(),
                parent_object: loser.parent_object,
                sequence: relay_core::Sequence(0),
                modified_by: loser.modified_by,
                modified_at_unix_ms: loser.modified_at_unix_ms,
                stat: None,
                materialized: false,
            });
        }
        let mut record = winner.clone();
        record.vector = local.vector.merged(&remote.vector);
        record.key = remote.key.clone();
        if winner_remote {
            record.stat = None;
            record.materialized = false;
        }
        let objects: Vec<(ObjectId, u64)> = [Some(&record), copy.as_ref()]
            .into_iter()
            .flatten()
            .filter_map(|row| match &row.content {
                EntryContent::File { object, size, .. } if self.store.contains(object) => {
                    Some((*object, *size))
                }
                _ => None,
            })
            .collect();
        let now = self.clock.now_ms();
        self.db.transaction(|repo| {
            for (id, size) in &objects {
                repo.record_object(*id, *size, now)?;
            }
            if let Some(mut copy) = copy {
                copy.sequence = repo.next_sequence()?;
                repo.put_entry(&copy)?;
            }
            record.sequence = repo.next_sequence()?;
            repo.put_entry(&record)?;
            Ok::<(), EngineError>(())
        })?;
        if record.content.is_deleted() {
            note_gone(mounts, remote);
        } else {
            note_live(mounts, remote);
        }
        Ok(())
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

        if remote.content.is_deleted() {
            return self.apply_deletion(remote, local, &root, mounts);
        }
        if cfg!(windows) && matches!(remote.content, EntryContent::Symlink { .. }) {
            return Ok(TryApply::Done(ApplyResult::Skipped(
                remote.key.path.to_string(),
                "symlinks are not materialized on Windows".into(),
            )));
        }
        let dest = dest_path(&root, &remote.key.path)?;
        if let Some(step) = clear_replaced_kind(
            local,
            remote.content.kind(),
            &remote.key.path,
            &dest,
            &self.store,
        )? {
            return Ok(step);
        }

        match &remote.content {
            EntryContent::File {
                object,
                size,
                executable,
            } => match self.materialize_remote_file(remote, local, &root, *object, *executable)? {
                MaterializeStep::Done(stat) => {
                    self.commit_remote(remote, stat, Some((*object, *size)), true)?;
                    note_live(mounts, remote);
                    Ok(TryApply::Done(ApplyResult::Written))
                }
                MaterializeStep::Rescan => Ok(TryApply::Rescan),
            },
            EntryContent::Directory => {
                if let Err(err) = ensure_real_dir_chain(&root, &dest) {
                    return skip_or_err(err);
                }
                self.commit_remote(remote, None, None, true)?;
                note_live(mounts, remote);
                Ok(TryApply::Done(ApplyResult::Written))
            }
            EntryContent::Symlink { target } => {
                if let Some(parent) = dest.parent()
                    && let Err(err) = ensure_real_dir_chain(&root, parent)
                {
                    return skip_or_err(err);
                }
                if !symlink_still_matches(local, &dest)? {
                    return Ok(TryApply::Rescan);
                }
                create_symlink(target, &dest)?;
                let stat = fs::symlink_metadata(&dest)
                    .ok()
                    .map(|m| StatHint::from_metadata(&m));
                self.commit_remote(remote, stat, None, true)?;
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
                self.commit_remote(remote, None, None, true)?;
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
                self.commit_remote(remote, None, None, true)?;
                note_gone(mounts, remote);
                return Ok(TryApply::Done(ApplyResult::Deleted));
            }
            Err(err) => return Err(EngineError::Io(err)),
        };

        if meta.is_dir() && !meta.file_type().is_symlink() {
            let indexed_dir = local.is_some_and(|record| {
                !record.is_deleted() && matches!(record.content, EntryContent::Directory)
            });
            if !indexed_dir {
                return Ok(TryApply::Rescan);
            }
            match fs::remove_dir(&dest) {
                Ok(()) => {
                    self.commit_remote(remote, None, None, true)?;
                    note_gone(mounts, remote);
                    Ok(TryApply::Done(ApplyResult::Deleted))
                }
                Err(_) => Ok(TryApply::Done(ApplyResult::Skipped(
                    remote.key.path.to_string(),
                    "directory is not empty; tombstone not stored".into(),
                ))),
            }
        } else {
            let still_matches = if meta.file_type().is_symlink() {
                symlink_still_matches(local, &dest)?
            } else {
                file_still_matches(local, &dest, &self.store)?
            };
            if !still_matches {
                return Ok(TryApply::Rescan);
            }
            fs::remove_file(&dest).map_err(EngineError::Io)?;
            self.commit_remote(remote, None, None, true)?;
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
        if let Some(done) = self.try_auto_merge(remote, local, mounts)? {
            return Ok(done);
        }
        let remote_rec = remote.clone().into_record(local.sequence, None, true);
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
                materialized: true,
            };
            copy_path = Some((path, copy));
        }

        let mut path_stat = None;
        if winner_remote {
            let dest = dest_path(&root, &remote.key.path)?;
            if let Some(step) = clear_replaced_kind(
                Some(local),
                winner.content.kind(),
                &remote.key.path,
                &dest,
                &self.store,
            )? {
                return Ok(step);
            }
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
                    ensure_real_dir_chain(&root, &dest)?;
                }
                other => {
                    if matches!(other, EntryContent::Symlink { .. })
                        && !symlink_still_matches(Some(local), &dest)?
                    {
                        return Ok(TryApply::Rescan);
                    }
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
                materialized: true,
            };
            repo.put_entry(&record)?;
            Ok::<(), EngineError>(())
        })?;

        note_live(mounts, remote);
        Ok(TryApply::Done(ApplyResult::Conflict))
    }

    /// Clean three-way merge of concurrent text edits that share a parent object.
    /// `None` means fall through to conflict copies (dirty merge, binary, git, or
    /// no shared base). A clean result is one new object and a merged vector,
    /// with no bump and no conflict copy, so both peers converge.
    fn try_auto_merge(
        &mut self,
        remote: &RemoteEntry,
        local: &EntryRecord,
        mounts: &mut HashMap<MountId, MountApply>,
    ) -> Result<Option<TryApply>, EngineError> {
        if local.vector.compare(&remote.vector) != VectorOrdering::Concurrent {
            return Ok(None);
        }
        if is_git_metadata(&remote.key.path) {
            return Ok(None);
        }
        let (
            EntryContent::File {
                object: local_object,
                ..
            },
            EntryContent::File {
                object: remote_object,
                ..
            },
        ) = (&local.content, &remote.content)
        else {
            return Ok(None);
        };
        let Some(base_id) = local.parent_object else {
            return Ok(None);
        };
        if remote.parent_object != Some(base_id) {
            return Ok(None);
        }
        if !self.store.contains(&base_id)
            || !self.store.contains(local_object)
            || !self.store.contains(remote_object)
        {
            return Ok(None);
        }
        let base = self.store.read(&base_id)?;
        let left = self.store.read(local_object)?;
        let right = self.store.read(remote_object)?;
        let MergeOutcome::Clean(merged) = merge_text(&base, &left, &right) else {
            return Ok(None);
        };

        let remote_rec = remote.clone().into_record(local.sequence, None, true);
        let (winner_remote, _) = conflict_outcome(local, &remote_rec);
        let winner = if winner_remote { &remote_rec } else { local };
        let executable = match &winner.content {
            EntryContent::File { executable, .. } => *executable,
            _ => false,
        };
        let merged_id = self.store.put_bytes(&merged)?;
        let ctx = mounts
            .get(&remote.key.mount)
            .ok_or(EngineError::MountNotLocal)?;
        let root = ctx
            .config
            .local_path
            .clone()
            .ok_or(EngineError::MountNotLocal)?;
        let path_stat = match self.materialize_remote_file(
            remote,
            Some(local),
            &root,
            merged_id,
            executable,
        )? {
            MaterializeStep::Done(stat) => stat,
            MaterializeStep::Rescan => return Ok(Some(TryApply::Rescan)),
        };
        let now = self.clock.now_ms();
        let size = merged.len() as u64;
        let content = EntryContent::File {
            object: merged_id,
            size,
            executable,
        };
        let merged_vector = local.vector.merged(&remote.vector);
        let path_key = remote.key.clone();
        let modified_by = winner.modified_by;
        let modified_at = winner.modified_at_unix_ms;
        self.db.transaction(|repo| {
            repo.record_object(merged_id, size, now)?;
            let sequence = repo.next_sequence()?;
            let record = EntryRecord {
                key: path_key,
                content,
                vector: merged_vector,
                parent_object: Some(base_id),
                sequence,
                modified_by,
                modified_at_unix_ms: modified_at,
                stat: path_stat,
                materialized: true,
            };
            repo.put_entry(&record)?;
            Ok::<(), EngineError>(())
        })?;
        note_live(mounts, remote);
        Ok(Some(TryApply::Done(ApplyResult::Written)))
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
            if dehydrated_stat(&dest).is_some() {
                return Ok(
                    if relay_fs::cloud::placeholder_object(&dest) == Some(*object) {
                        Ok(())
                    } else {
                        Err(TryApply::Done(ApplyResult::Skipped(
                            path.to_string(),
                            "conflict copy path is already taken".into(),
                        )))
                    },
                );
            }
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
        materialized: bool,
    ) -> Result<(), EngineError> {
        let now = self.clock.now_ms();
        let remote = remote.clone();
        let candidate = object.or_else(|| {
            remote.content.object().map(|id| {
                let size = match &remote.content {
                    EntryContent::File { size, .. } => *size,
                    _ => 0,
                };
                (id, size)
            })
        });
        let record_object = candidate.filter(|(id, _)| materialized || self.store.contains(id));
        let stat = materialized.then_some(stat).flatten();
        self.db.transaction(|repo| {
            if let Some((id, size)) = record_object {
                repo.record_object(id, size, now)?;
            }
            let sequence = repo.next_sequence()?;
            let record = remote.into_record(sequence, stat, materialized);
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

fn skip_reason(entry: &RemoteEntry, ctx: &MountApply, writing: bool) -> Option<String> {
    if ctx.config.local_path.is_none() {
        return Some("mount is not attached on this device".into());
    }
    if is_reserved(&entry.key.path) {
        return Some("reserved path".into());
    }
    if writing && cfg!(windows) && matches!(entry.content, EntryContent::Symlink { .. }) {
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
        && let Some(existing) = ctx
            .live_fold
            .as_ref()
            .and_then(|fold| fold.get(&entry.key.path.case_fold_key()))
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
    is_bookkeeping_path(path)
}

pub(crate) fn dest_path(root: &Path, path: &LogicalPath) -> Result<PathBuf, EngineError> {
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
    if let Some(stat) = dehydrated_stat(dest) {
        // Never read a placeholder without data. Replace it when it holds
        // this version, the local one, or what a deleted row left behind;
        // otherwise the scanner indexes it first.
        let object = relay_fs::cloud::placeholder_object(dest);
        let known = object.is_some()
            && (object == Some(remote_object)
                || local.is_some_and(|record| {
                    record.content.object() == object
                        || (record.is_deleted() && record.parent_object == object)
                }));
        return Ok(if known {
            ExpectedPrep::Matches(Some(stat))
        } else {
            ExpectedPrep::Rescan
        });
    }
    if let Some(record) = local
        && !record.is_deleted()
        && matches!(record.content, EntryContent::File { .. })
    {
        return match record.stat {
            Some(stat) => Ok(ExpectedPrep::Matches(Some(stat))),
            None => hash_or_rescan(store, dest, record.content.object(), remote_object),
        };
    }
    // No live file record: only a file that already holds the remote bytes
    // may be replaced. A directory or link here is an unscanned change.
    match fs::symlink_metadata(dest) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
            hash_or_rescan(store, dest, None, remote_object)
        }
        Ok(_) => Ok(ExpectedPrep::Rescan),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(ExpectedPrep::Matches(None)),
        Err(err) => Err(EngineError::Io(err)),
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

/// Whether the file at `dest` is still the one the local record describes,
/// so it can be removed or replaced without losing anything the scanner has
/// not indexed. Nothing on disk always matches.
///
/// With no live local record, any file present is unindexed and never
/// matches, except the placeholder a deleted row left behind (D43).
fn file_still_matches(
    local: Option<&EntryRecord>,
    dest: &Path,
    store: &relay_store::ObjectStore,
) -> Result<bool, EngineError> {
    let meta = match fs::symlink_metadata(dest) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(EngineError::Io(err)),
    };
    let Some(record) = local.filter(|record| !record.is_deleted()) else {
        let leftover = local.is_some_and(|record| {
            record.parent_object.is_some()
                && relay_fs::cloud::is_dehydrated(&meta)
                && relay_fs::cloud::placeholder_object(dest) == record.parent_object
        });
        return Ok(leftover);
    };
    if !matches!(record.content, EntryContent::File { .. })
        || !meta.is_file()
        || meta.file_type().is_symlink()
    {
        return Ok(false);
    }
    match record.stat {
        Some(stat) => Ok(StatHint::from_metadata(&meta) == stat),
        None if relay_fs::cloud::is_dehydrated(&meta) => {
            Ok(relay_fs::cloud::placeholder_object(dest) == record.content.object())
        }
        None => match store.hash_file(dest, None) {
            Ok(outcome) => Ok(Some(outcome.id) == record.content.object()),
            Err(StoreError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                Ok(true)
            }
            Err(StoreError::SourceChanged { .. }) => Ok(false),
            Err(err) => Err(err.into()),
        },
    }
}

/// Whether the symlink at `dest` is the one the local record describes.
/// Nothing on disk always matches; a link with no live local record never does.
fn symlink_still_matches(local: Option<&EntryRecord>, dest: &Path) -> Result<bool, EngineError> {
    match fs::symlink_metadata(dest) {
        Ok(meta) if !meta.file_type().is_symlink() => return Ok(false),
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(EngineError::Io(err)),
    }
    let Some(EntryContent::Symlink { target }) = local
        .filter(|record| !record.is_deleted())
        .map(|r| &r.content)
    else {
        return Ok(false);
    };
    Ok(fs::read_link(dest).map_err(EngineError::Io)?.as_os_str() == target.as_str())
}

/// Clear the path for a remote version of another kind than the live local
/// one: remove the file, link or empty directory the local record describes.
/// Anything else on disk is an unscanned change and the scanner indexes it
/// first (`Rescan`). A directory that is not empty stays, as for its tombstone.
fn clear_replaced_kind(
    local: Option<&EntryRecord>,
    remote_kind: Option<EntryKind>,
    path: &LogicalPath,
    dest: &Path,
    store: &relay_store::ObjectStore,
) -> Result<Option<TryApply>, EngineError> {
    let Some(record) = local.filter(|record| !record.is_deleted()) else {
        return Ok(None);
    };
    if remote_kind.is_none() || record.content.kind() == remote_kind {
        return Ok(None);
    }
    let meta = match fs::symlink_metadata(dest) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(EngineError::Io(err)),
    };
    match &record.content {
        EntryContent::Directory => {
            if !meta.is_dir() || meta.file_type().is_symlink() {
                return Ok(Some(TryApply::Rescan));
            }
            match fs::remove_dir(dest) {
                Ok(()) => Ok(None),
                Err(_) => Ok(Some(TryApply::Done(ApplyResult::Skipped(
                    path.to_string(),
                    "directory is not empty; replacement not applied".into(),
                )))),
            }
        }
        EntryContent::File { .. } => {
            if !file_still_matches(Some(record), dest, store)? {
                return Ok(Some(TryApply::Rescan));
            }
            fs::remove_file(dest).map_err(EngineError::Io)?;
            Ok(None)
        }
        EntryContent::Symlink { .. } => {
            if !symlink_still_matches(Some(record), dest)? {
                return Ok(Some(TryApply::Rescan));
            }
            fs::remove_file(dest).map_err(EngineError::Io)?;
            Ok(None)
        }
        EntryContent::Deleted => Ok(None),
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

/// Create (or replace) the symlink at `dest`. Only an existing symlink is
/// replaced; callers clear a file or directory first, after checking it
/// still matches the index.
pub(crate) fn create_symlink(target: &str, dest: &Path) -> Result<(), EngineError> {
    #[cfg(unix)]
    {
        match fs::symlink_metadata(dest) {
            Ok(meta) if meta.file_type().is_symlink() => {
                fs::remove_file(dest).map_err(EngineError::Io)?;
            }
            Ok(_) => {
                return Err(EngineError::Io(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a symlink", dest.display()),
                )));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(EngineError::Io(err)),
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
        && let Some(fold) = ctx.live_fold.as_mut()
        && !remote.content.is_deleted()
    {
        fold.insert(remote.key.path.case_fold_key(), remote.key.path.clone());
    }
}

fn note_gone(mounts: &mut HashMap<MountId, MountApply>, remote: &RemoteEntry) {
    if let Some(ctx) = mounts.get_mut(&remote.key.mount)
        && let Some(fold) = ctx.live_fold.as_mut()
    {
        fold.remove(&remote.key.path.case_fold_key());
    }
}

fn skip_or_err<T>(err: relay_fs::FsError) -> Result<T, EngineError> {
    Err(EngineError::Fs(err))
}

/// An I/O failure while writing the working tree or the store is retried
/// (the batch stays queued) rather than skipped: a full disk, a locked file
/// or a flaky volume must not drop an update for good, which would leave
/// this replica silently diverged until the path changes again. Only errors
/// that describe the request itself, not the device, are final.
fn is_transient(err: &EngineError) -> bool {
    match err {
        EngineError::Io(source)
        | EngineError::Fs(relay_fs::FsError::Io { source, .. })
        | EngineError::Store(StoreError::Io { source, .. }) => !matches!(
            source.kind(),
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData | io::ErrorKind::Unsupported
        ),
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
