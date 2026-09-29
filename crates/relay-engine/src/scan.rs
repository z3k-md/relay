use std::collections::{HashMap, HashSet};
use std::path::Path;

use relay_core::{
    EntryContent, EntryKey, EntryKind, EntryRecord, LocalChange, LogicalPath, Observation,
    derive_local_change, needs_rehash,
};
use relay_fs::{ScanWarning, effective_rules, scan_mount, to_logical_path};
use relay_policy::MountRules;
use relay_store::StoreError;

use crate::Engine;
use crate::error::EngineError;
use crate::reports::{ScanOptions, ScanReport, Warning, is_mass_delete};

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

enum ChangeKind {
    Created,
    Modified,
    Deleted,
}

impl Engine {
    pub(crate) fn scan_mount(
        &mut self,
        space_name: &str,
        mount_name: &str,
        opts: ScanOptions,
    ) -> Result<ScanReport, EngineError> {
        let (space, config) = self.lookup_mount(space_name, mount_name)?;
        let local_path = config
            .local_path
            .clone()
            .ok_or(EngineError::MountNotLocal)?;
        relay_fs::MountMarker::verify(&local_path, config.mount.id)?;

        let user_rules = MountRules::new(&config.includes, &config.excludes)?;
        let scanned = scan_mount(&local_path, &user_rules)?;
        let rules = match scanned.rules.clone() {
            Some(rules) => rules,
            None => effective_rules(&local_path, &user_rules, &mut Vec::new())?,
        };

        let previous = self.db.repo().entries_for_mount(config.mount.id)?;
        let prev_by_path: HashMap<LogicalPath, EntryRecord> = previous
            .into_iter()
            .map(|record| (record.key.path.clone(), record))
            .collect();

        let protection = protected_prefixes(&local_path, &scanned.warnings);
        let mut report = ScanReport {
            warnings: scanned.warnings.iter().map(Warning::from).collect(),
            ..ScanReport::default()
        };

        let mut scanned_paths = HashSet::new();
        let mut unstable = HashSet::new();
        let mut writes = Vec::new();
        let mut stat_updates = Vec::new();
        let mut new_objects = Vec::new();

        for entry in &scanned.entries {
            scanned_paths.insert(entry.path.clone());
            let prev = prev_by_path.get(&entry.path);
            let observation = match observation_for(
                &self.store,
                entry,
                prev,
                &mut unstable,
                &mut report.warnings,
                &mut new_objects,
                &mut report.bytes_hashed,
            )? {
                Some(obs) => obs,
                None => continue,
            };

            match derive_local_change(prev, Some(&observation)) {
                LocalChange::Created => {
                    writes.push((
                        ChangeKind::Created,
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
                        },
                    ));
                }
                LocalChange::Modified => {
                    writes.push((
                        ChangeKind::Modified,
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
                        },
                    ));
                }
                LocalChange::StatOnly => {
                    report.stat_only += 1;
                    if let Some(prev) = prev {
                        stat_updates.push((prev.key.clone(), observation.stat));
                    }
                }
                LocalChange::Unchanged => report.unchanged += 1,
                LocalChange::Deleted => {}
            }
        }

        let mut deletion_candidates = Vec::new();
        for (path, record) in &prev_by_path {
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
            if let Some(kind) = record.content.kind() {
                if is_deselected(&rules, path, kind) {
                    report.deselected.push(path.clone());
                    continue;
                }
            }
            if unstable.contains(path) {
                continue;
            }
            deletion_candidates.push(record.clone());
        }

        let live_count = prev_by_path.values().filter(|r| !r.is_deleted()).count();
        if !opts.allow_mass_delete
            && is_mass_delete(deletion_candidates.len(), live_count, scanned.entries.len())
        {
            return Err(EngineError::MassDeleteRefused {
                deletions: deletion_candidates.len(),
                live: live_count,
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

        let now = self.clock.now_ms();
        let device = self.device.id;
        self.db
            .transaction(|repo| {
                for (id, size) in &new_objects {
                    repo.record_object(*id, *size, now)?;
                }
                for (kind, write) in &writes {
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
                    match kind {
                        ChangeKind::Created => report.created += 1,
                        ChangeKind::Modified => report.modified += 1,
                        ChangeKind::Deleted => report.deleted += 1,
                    }
                }
                for (key, stat) in &stat_updates {
                    repo.update_stat(key, *stat)?;
                }
                Ok::<(), EngineError>(())
            })
            .map_err(EngineError::from_db_keep)?;

        Ok(report)
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

fn observation_for(
    store: &relay_store::ObjectStore,
    entry: &relay_fs::ScannedEntry,
    prev: Option<&EntryRecord>,
    unstable: &mut HashSet<LogicalPath>,
    warnings: &mut Vec<Warning>,
    new_objects: &mut Vec<(relay_core::ObjectId, u64)>,
    bytes_hashed: &mut u64,
) -> Result<Option<Observation>, EngineError> {
    match entry.kind {
        EntryKind::File => {
            if needs_rehash(prev, &entry.stat) {
                match store.put_file(&entry.os_path, Some(&entry.stat)) {
                    Ok(outcome) => {
                        *bytes_hashed += outcome.size;
                        new_objects.push((outcome.id, outcome.size));
                        Ok(Some(Observation {
                            content: EntryContent::File {
                                object: outcome.id,
                                size: outcome.size,
                                executable: entry.executable,
                            },
                            stat: Some(outcome.stat),
                        }))
                    }
                    Err(StoreError::SourceChanged { .. }) => {
                        unstable.insert(entry.path.clone());
                        Ok(None)
                    }
                    // Locked or permission-denied user files (common on
                    // Windows) must not abort the whole mount; errors on the
                    // store's own paths still do.
                    Err(StoreError::Io { path, source }) if path == entry.os_path => {
                        warnings.push(Warning {
                            message: format!("could not read {}: {source}", path.display()),
                        });
                        unstable.insert(entry.path.clone());
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
                    stat: Some(entry.stat),
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
            stat: Some(entry.stat),
        })),
    }
}

fn protected_prefixes(root: &Path, warnings: &[ScanWarning]) -> Protection {
    let mut prefixes = Vec::new();
    for warning in warnings {
        let os_path = match warning {
            ScanWarning::Unreadable { os_path, .. }
            | ScanWarning::NonUtf8Name(os_path)
            | ScanWarning::InvalidName { os_path, .. }
            | ScanWarning::NestedMount(os_path) => os_path.as_path(),
            _ => continue,
        };
        match logical_or_ancestor(root, os_path) {
            None => return Protection::EntireMount,
            Some(path) => prefixes.push(path),
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
