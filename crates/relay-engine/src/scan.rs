use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use relay_core::{
    EntryContent, EntryKey, EntryKind, EntryRecord, LocalChange, LogicalPath, MountId, Observation,
    Sequence, derive_local_change, needs_rehash,
};
use relay_fs::{ScanScope, ScanWarning, ScopeKind, effective_rules, scan_mount, to_logical_path};
use relay_policy::MountRules;
use relay_store::StoreError;

use crate::Engine;
use crate::error::EngineError;
use crate::reports::{ScanOptions, ScanReport, Warning, is_large_fraction_delete, is_mass_delete};

const SCAN_APPLY_ATTEMPTS: u32 = 3;

enum Protection {
    EntireMount,
    Prefixes(Vec<LogicalPath>),
}

struct VersionWrite {
    path: LogicalPath,
    previous: Option<EntryRecord>,
    key: EntryKey,
    content: EntryContent,
    stat: Option<relay_core::StatHint>,
}

struct StatUpdate {
    key: EntryKey,
    stat: Option<relay_core::StatHint>,
    previous_sequence: Sequence,
}

enum ChangeKind {
    Created,
    Modified,
    Deleted,
}

pub(crate) struct ScanPlan {
    writes: Vec<(ChangeKind, VersionWrite)>,
    stat_updates: Vec<StatUpdate>,
    new_objects: Vec<(relay_core::ObjectId, u64)>,
    report: ScanReport,
}

impl Engine {
    pub(crate) fn scan_mount(
        &mut self,
        space_name: &str,
        mount_name: &str,
        opts: ScanOptions,
    ) -> Result<ScanReport, EngineError> {
        self.run_scan(space_name, mount_name, opts, true, None)
    }

    pub(crate) fn scan_paths_inner(
        &mut self,
        space: &str,
        mount: &str,
        paths: &[LogicalPath],
        opts: ScanOptions,
    ) -> Result<ScanReport, EngineError> {
        self.run_scan(space, mount, opts, false, Some(paths))
    }

    fn run_scan(
        &mut self,
        space_name: &str,
        mount_name: &str,
        opts: ScanOptions,
        full: bool,
        paths: Option<&[LogicalPath]>,
    ) -> Result<ScanReport, EngineError> {
        self.ensure_writable()?;
        let (space, config) = self.lookup_mount(space_name, mount_name)?;
        let mount_id = config.mount.id;
        let result = self.scan_attempts(&space, &config, opts, full, paths);
        if !opts.dry_run {
            self.record_scan_bookkeeping(mount_id, full, &result)?;
        }
        result
    }

    fn scan_attempts(
        &mut self,
        space: &relay_core::Space,
        config: &relay_db::MountConfig,
        opts: ScanOptions,
        full: bool,
        paths: Option<&[LogicalPath]>,
    ) -> Result<ScanReport, EngineError> {
        let mut last_conflict = None;
        for attempt in 0..SCAN_APPLY_ATTEMPTS {
            let plan = if full {
                self.plan_scan_for(space, config, opts)?
            } else {
                self.plan_scan_paths_for(space, config, paths.unwrap_or(&[]), opts)?
            };
            if opts.dry_run {
                return Ok(plan.report);
            }
            match self.apply_scan(&plan) {
                Ok(report) => return Ok(report),
                Err(err @ EngineError::ConcurrentModification { .. })
                    if attempt + 1 < SCAN_APPLY_ATTEMPTS =>
                {
                    last_conflict = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_conflict.expect("retry loop only continues on ConcurrentModification"))
    }

    #[cfg(test)]
    pub(crate) fn plan_scan(
        &self,
        space_name: &str,
        mount_name: &str,
        opts: ScanOptions,
    ) -> Result<ScanPlan, EngineError> {
        let (space, config) = self.lookup_mount(space_name, mount_name)?;
        self.plan_scan_for(&space, &config, opts)
    }

    fn plan_scan_for(
        &self,
        space: &relay_core::Space,
        config: &relay_db::MountConfig,
        opts: ScanOptions,
    ) -> Result<ScanPlan, EngineError> {
        let local_path = verified_root(config)?;
        let user_rules = MountRules::new(&config.includes, &config.excludes)?;
        let scanned = scan_mount(&local_path, &user_rules)?;
        let rules = match scanned.rules.clone() {
            Some(rules) => rules,
            None => effective_rules(&local_path, &user_rules, &mut Vec::new())?,
        };
        let previous = self.db.repo().entries_for_mount(config.mount.id)?;
        let live_count = previous.iter().filter(|r| !r.is_deleted()).count();
        let deletion_scope: HashSet<LogicalPath> = previous
            .iter()
            .filter(|r| !r.is_deleted())
            .map(|r| r.key.path.clone())
            .collect();
        self.build_plan(BuildPlan {
            space,
            config,
            opts,
            local_path: &local_path,
            entries: &scanned.entries,
            warnings: &scanned.warnings,
            rules: &rules,
            previous,
            deletion_scope,
            live_for_guard: live_count,
            empty_scan_rule: true,
        })
    }

    fn plan_scan_paths_for(
        &self,
        space: &relay_core::Space,
        config: &relay_db::MountConfig,
        paths: &[LogicalPath],
        opts: ScanOptions,
    ) -> Result<ScanPlan, EngineError> {
        let local_path = verified_root(config)?;
        let user_rules = MountRules::new(&config.includes, &config.excludes)?;
        let scanned = relay_fs::scan_paths(&local_path, &user_rules, paths)?;
        let previous = self.db.repo().entries_for_mount(config.mount.id)?;
        let deletion_scope = scoped_previous_paths(self, config.mount.id, &scanned.scopes)?;
        let live_for_guard = self.db.repo().count_live(config.mount.id)?;
        self.build_plan(BuildPlan {
            space,
            config,
            opts,
            local_path: &local_path,
            entries: &scanned.entries,
            warnings: &scanned.warnings,
            rules: &scanned.rules,
            previous,
            deletion_scope,
            live_for_guard,
            empty_scan_rule: false,
        })
    }

    fn build_plan(&self, input: BuildPlan<'_>) -> Result<ScanPlan, EngineError> {
        let BuildPlan {
            space,
            config,
            opts,
            local_path,
            entries,
            warnings,
            rules,
            previous,
            deletion_scope,
            live_for_guard,
            empty_scan_rule,
        } = input;

        let prev_by_path: HashMap<LogicalPath, EntryRecord> = previous
            .into_iter()
            .map(|record| (record.key.path.clone(), record))
            .collect();

        let protection = protected_prefixes(local_path, warnings);
        let mut report = ScanReport {
            warnings: warnings.iter().map(Warning::from).collect(),
            ..ScanReport::default()
        };

        let mut scanned_paths = HashSet::new();
        let mut unstable = HashSet::new();
        let mut writes = Vec::new();
        let mut stat_updates = Vec::new();
        let mut new_objects = Vec::new();
        let wall_now_ns = wall_clock_now_ns();

        for entry in entries {
            scanned_paths.insert(entry.path.clone());
            let prev = prev_by_path.get(&entry.path);
            let mut observe = ObserveCtx {
                store: &self.store,
                unstable: &mut unstable,
                warnings: &mut report.warnings,
                new_objects: &mut new_objects,
                bytes_hashed: &mut report.bytes_hashed,
                wall_now_ns,
                racy_window: self.config.racy_window,
                dry_run: opts.dry_run,
            };
            let observation = match observation_for(&mut observe, entry, prev)? {
                Some(obs) => obs,
                None => continue,
            };

            match derive_local_change(prev, Some(&observation)) {
                LocalChange::Created => {
                    writes.push((
                        ChangeKind::Created,
                        version_write(space, config, entry, prev, observation),
                    ));
                }
                LocalChange::Modified => {
                    writes.push((
                        ChangeKind::Modified,
                        version_write(space, config, entry, prev, observation),
                    ));
                }
                LocalChange::StatOnly => {
                    report.stat_only += 1;
                    if let Some(prev) = prev {
                        stat_updates.push(StatUpdate {
                            key: prev.key.clone(),
                            stat: observation.stat,
                            previous_sequence: prev.sequence,
                        });
                    }
                }
                LocalChange::Unchanged => report.unchanged += 1,
                LocalChange::Deleted => {}
            }
        }

        let mut deletion_candidates = Vec::new();
        for path in &deletion_scope {
            let Some(record) = prev_by_path.get(path) else {
                continue;
            };
            if record.is_deleted() {
                continue;
            }
            if scanned_paths.contains(path) {
                continue;
            }
            if is_protected(&protection, path) {
                report.protected += 1;
                continue;
            }
            if let Some(kind) = record.content.kind()
                && is_deselected(rules, path, kind)
            {
                report.deselected.push(path.clone());
                continue;
            }
            if unstable.contains(path) {
                continue;
            }
            deletion_candidates.push(record.clone());
        }

        let refuse = if empty_scan_rule {
            is_mass_delete(deletion_candidates.len(), live_for_guard, entries.len())
        } else {
            is_large_fraction_delete(deletion_candidates.len(), live_for_guard)
        };
        if !opts.allow_mass_delete && refuse {
            return Err(EngineError::MassDeleteRefused {
                deletions: deletion_candidates.len(),
                live: live_for_guard,
            });
        }

        for record in deletion_candidates {
            writes.push((
                ChangeKind::Deleted,
                VersionWrite {
                    path: record.key.path.clone(),
                    previous: Some(record.clone()),
                    key: record.key.clone(),
                    content: EntryContent::Deleted,
                    stat: None,
                },
            ));
        }

        writes.sort_by(|a, b| a.1.path.cmp(&b.1.path));
        report.unstable = {
            let mut paths: Vec<_> = unstable.into_iter().collect();
            paths.sort();
            paths
        };
        report.deselected.sort();

        for (kind, _) in &writes {
            match kind {
                ChangeKind::Created => report.created += 1,
                ChangeKind::Modified => report.modified += 1,
                ChangeKind::Deleted => report.deleted += 1,
            }
        }

        Ok(ScanPlan {
            writes,
            stat_updates,
            new_objects,
            report,
        })
    }

    pub(crate) fn apply_scan(&mut self, plan: &ScanPlan) -> Result<ScanReport, EngineError> {
        let now = self.clock.now_ms();
        let device = self.device.id;
        self.db
            .transaction(|repo| {
                for write in plan.writes.iter().map(|(_, write)| write) {
                    let current = repo.entry(&write.key)?;
                    if !sequences_match(write.previous.as_ref(), current.as_ref()) {
                        return Err(EngineError::ConcurrentModification {
                            path: write.path.clone(),
                        });
                    }
                }
                for update in &plan.stat_updates {
                    let current = repo.entry(&update.key)?;
                    if current.as_ref().map(|c| c.sequence) != Some(update.previous_sequence) {
                        return Err(EngineError::ConcurrentModification {
                            path: update.key.path.clone(),
                        });
                    }
                }
                for (id, size) in &plan.new_objects {
                    repo.record_object(*id, *size, now)?;
                }
                for (_, write) in &plan.writes {
                    let sequence = repo.next_sequence()?;
                    let record = EntryRecord::local_write(
                        write.previous.as_ref(),
                        write.key.clone(),
                        write.content.clone(),
                        write.stat,
                        device,
                        now,
                        sequence,
                    );
                    repo.put_entry(&record)?;
                }
                for update in &plan.stat_updates {
                    repo.update_stat(&update.key, update.stat)?;
                }
                Ok(plan.report.clone())
            })
            .map_err(EngineError::from_db_keep)
    }

    fn record_scan_bookkeeping(
        &mut self,
        mount: MountId,
        full: bool,
        result: &Result<ScanReport, EngineError>,
    ) -> Result<(), EngineError> {
        let now = self.clock.now_ms();
        match result {
            Ok(_) => self
                .db
                .transaction(|repo| repo.record_scan_success(mount, full, now))
                .map_err(EngineError::from_db),
            Err(err) if is_mount_scan_error(err) => {
                let message = err.to_string();
                let _ = self
                    .db
                    .transaction(|repo| repo.record_scan_error(mount, &message, now));
                Ok(())
            }
            Err(_) => Ok(()),
        }
    }
}

impl EngineError {
    fn from_db_keep(err: EngineError) -> EngineError {
        match err {
            EngineError::Db(inner) => EngineError::from_db(inner),
            other => other,
        }
    }
}

struct BuildPlan<'a> {
    space: &'a relay_core::Space,
    config: &'a relay_db::MountConfig,
    opts: ScanOptions,
    local_path: &'a Path,
    entries: &'a [relay_fs::ScannedEntry],
    warnings: &'a [ScanWarning],
    rules: &'a MountRules,
    previous: Vec<EntryRecord>,
    deletion_scope: HashSet<LogicalPath>,
    live_for_guard: usize,
    empty_scan_rule: bool,
}

fn verified_root(config: &relay_db::MountConfig) -> Result<std::path::PathBuf, EngineError> {
    let local_path = config
        .local_path
        .clone()
        .ok_or(EngineError::MountNotLocal)?;
    relay_fs::MountMarker::verify(&local_path, config.mount.id)?;
    Ok(local_path)
}

fn scoped_previous_paths(
    engine: &Engine,
    mount: MountId,
    scopes: &[ScanScope],
) -> Result<HashSet<LogicalPath>, EngineError> {
    let mut out = HashSet::new();
    for scope in scopes {
        match scope.kind {
            ScopeKind::Exact => {
                out.insert(scope.path.clone());
            }
            ScopeKind::Subtree => {
                for record in engine.db.repo().entries_under(mount, &scope.path)? {
                    out.insert(record.key.path);
                }
            }
        }
    }
    Ok(out)
}

fn version_write(
    space: &relay_core::Space,
    config: &relay_db::MountConfig,
    entry: &relay_fs::ScannedEntry,
    prev: Option<&EntryRecord>,
    observation: Observation,
) -> VersionWrite {
    VersionWrite {
        path: entry.path.clone(),
        previous: prev.cloned(),
        key: EntryKey {
            space: space.id,
            mount: config.mount.id,
            path: entry.path.clone(),
        },
        content: observation.content,
        stat: observation.stat,
    }
}

fn sequences_match(previous: Option<&EntryRecord>, current: Option<&EntryRecord>) -> bool {
    match (previous, current) {
        (None, None) => true,
        (Some(prev), Some(cur)) => prev.sequence == cur.sequence,
        _ => false,
    }
}

fn is_mount_scan_error(err: &EngineError) -> bool {
    match err {
        EngineError::MassDeleteRefused { .. } | EngineError::Io(_) => true,
        EngineError::Fs(fs) => matches!(
            fs,
            relay_fs::FsError::MarkerMissing(_)
                | relay_fs::FsError::MarkerMismatch { .. }
                | relay_fs::FsError::MountRootMissing(_)
                | relay_fs::FsError::Io { .. }
        ),
        _ => false,
    }
}

struct ObserveCtx<'a> {
    store: &'a relay_store::ObjectStore,
    unstable: &'a mut HashSet<LogicalPath>,
    warnings: &'a mut Vec<Warning>,
    new_objects: &'a mut Vec<(relay_core::ObjectId, u64)>,
    bytes_hashed: &'a mut u64,
    wall_now_ns: i64,
    racy_window: Duration,
    dry_run: bool,
}

fn observation_for(
    ctx: &mut ObserveCtx<'_>,
    entry: &relay_fs::ScannedEntry,
    prev: Option<&EntryRecord>,
) -> Result<Option<Observation>, EngineError> {
    match entry.kind {
        EntryKind::File => {
            if needs_rehash(prev, &entry.stat) {
                let hashed = if ctx.dry_run {
                    ctx.store.hash_file(&entry.os_path, Some(&entry.stat))
                } else {
                    ctx.store.put_file(&entry.os_path, Some(&entry.stat))
                };
                match hashed {
                    Ok(outcome) => {
                        *ctx.bytes_hashed += outcome.size;
                        if !ctx.dry_run {
                            ctx.new_objects.push((outcome.id, outcome.size));
                        }
                        Ok(Some(Observation {
                            content: EntryContent::File {
                                object: outcome.id,
                                size: outcome.size,
                                executable: entry.executable,
                            },
                            stat: recorded_stat(outcome.stat, ctx.wall_now_ns, ctx.racy_window),
                        }))
                    }
                    Err(StoreError::SourceChanged { .. }) => {
                        ctx.unstable.insert(entry.path.clone());
                        Ok(None)
                    }
                    // Locked or permission-denied user files (common on
                    // Windows) must not abort the whole mount; errors on the
                    // store's own paths still do.
                    Err(StoreError::Io { path, source }) if path == entry.os_path => {
                        ctx.warnings.push(Warning {
                            message: format!("could not read {}: {source}", path.display()),
                        });
                        ctx.unstable.insert(entry.path.clone());
                        Ok(None)
                    }
                    Err(err) => Err(err.into()),
                }
            } else if let Some(EntryContent::File { object, size, .. }) = prev.map(|p| &p.content) {
                Ok(Some(Observation {
                    content: EntryContent::File {
                        object: *object,
                        size: *size,
                        executable: entry.executable,
                    },
                    stat: recorded_stat(entry.stat, ctx.wall_now_ns, ctx.racy_window),
                }))
            } else {
                Ok(None)
            }
        }
        EntryKind::Directory => Ok(Some(Observation {
            content: EntryContent::Directory,
            stat: None,
        })),
        EntryKind::Symlink => Ok(Some(Observation {
            content: EntryContent::Symlink {
                target: entry.symlink_target.clone().unwrap_or_default(),
            },
            stat: recorded_stat(entry.stat, ctx.wall_now_ns, ctx.racy_window),
        })),
    }
}

pub(crate) fn wall_clock_now_ns() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

pub(crate) fn recorded_stat(
    stat: relay_core::StatHint,
    wall_now_ns: i64,
    racy_window: Duration,
) -> Option<relay_core::StatHint> {
    if racy_window.is_zero() {
        return Some(stat);
    }
    let window_ns = i64::try_from(racy_window.as_nanos()).unwrap_or(i64::MAX);
    if wall_now_ns.saturating_sub(stat.mtime_ns) < window_ns {
        None
    } else {
        Some(stat)
    }
}

fn protected_prefixes(root: &Path, warnings: &[ScanWarning]) -> Protection {
    let mut prefixes = Vec::new();
    for warning in warnings {
        match warning {
            ScanWarning::Unreadable { os_path, .. }
            | ScanWarning::NonUtf8Name(os_path)
            | ScanWarning::InvalidName { os_path, .. }
            | ScanWarning::NestedMount(os_path)
            | ScanWarning::SpecialFile(os_path) => match logical_or_ancestor(root, os_path) {
                None => return Protection::EntireMount,
                Some(path) => prefixes.push(path),
            },
            ScanWarning::NormalizationCollision { path, .. } => prefixes.push(path.clone()),
            _ => continue,
        }
    }
    prefixes.sort();
    prefixes.dedup();
    Protection::Prefixes(prefixes)
}

fn logical_or_ancestor(root: &Path, os_path: &Path) -> Option<LogicalPath> {
    let mut current = os_path;
    loop {
        if current == root {
            return None;
        }
        if let Ok(path) = to_logical_path(root, current) {
            return Some(path);
        }
        current = current.parent()?;
        if current == root {
            return None;
        }
    }
}

fn is_protected(protection: &Protection, path: &LogicalPath) -> bool {
    match protection {
        Protection::EntireMount => true,
        Protection::Prefixes(prefixes) => prefixes.iter().any(|prefix| path.starts_with(prefix)),
    }
}

fn is_deselected(rules: &MountRules, path: &LogicalPath, kind: EntryKind) -> bool {
    if !rules.is_selected(path, kind) {
        return true;
    }
    // The scanner skips a directory when it must not descend, even if the
    // directory path itself still matches an include (e.g. `**/node_modules/**`).
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;

    use relay_core::{EntryContent, EntryRecord, ObjectId};
    use tempfile::TempDir;

    use crate::{Engine, EngineConfig, ManualClock};

    fn ready() -> (TempDir, TempDir, Engine) {
        let home = TempDir::new().unwrap();
        let mount = TempDir::new().unwrap();
        let mut engine = Engine::init(home.path(), "testdev")
            .unwrap()
            .with_clock(Arc::new(ManualClock::new(1_700_000_000_000)))
            .with_config(EngineConfig {
                racy_window: Duration::ZERO,
            });
        engine.create_space("Personal").unwrap();
        engine
            .add_mount("Personal", "code", mount.path(), &[], &[])
            .unwrap();
        (home, mount, engine)
    }

    #[test]
    fn apply_detects_concurrent_modification_and_writes_nothing() {
        let (_home, mount, mut engine) = ready();
        fs::write(mount.path().join("a.txt"), b"v1").unwrap();
        engine
            .scan("Personal", "code", ScanOptions::default())
            .unwrap();

        fs::write(mount.path().join("a.txt"), b"v2").unwrap();
        let plan = engine
            .plan_scan("Personal", "code", ScanOptions::default())
            .unwrap();
        assert_eq!(plan.report.modified, 1);

        let current = engine
            .entries("Personal", "code", false)
            .unwrap()
            .into_iter()
            .find(|e| e.key.path.as_str() == "a.txt")
            .unwrap();
        let seq_before = current.sequence;
        let now = engine.clock.now_ms();
        let device = engine.device.id;
        engine
            .db
            .transaction(|repo| {
                let sequence = repo.next_sequence()?;
                let record = EntryRecord::local_write(
                    Some(&current),
                    current.key.clone(),
                    EntryContent::File {
                        object: ObjectId::of(b"sneak"),
                        size: 5,
                        executable: false,
                    },
                    current.stat,
                    device,
                    now,
                    sequence,
                );
                repo.put_entry(&record)?;
                Ok::<(), EngineError>(())
            })
            .unwrap();

        let err = engine.apply_scan(&plan).unwrap_err();
        assert!(
            matches!(
                err,
                EngineError::ConcurrentModification { ref path } if path.as_str() == "a.txt"
            ),
            "{err}"
        );

        let after = engine
            .entries("Personal", "code", false)
            .unwrap()
            .into_iter()
            .find(|e| e.key.path.as_str() == "a.txt")
            .unwrap();
        assert_eq!(after.content.object(), Some(ObjectId::of(b"sneak")));
        assert_ne!(after.sequence, seq_before);
        assert_eq!(
            after.content,
            EntryContent::File {
                object: ObjectId::of(b"sneak"),
                size: 5,
                executable: false,
            }
        );
    }
}
