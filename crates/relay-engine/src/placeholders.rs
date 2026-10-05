//! Online-only files as Cloud Files placeholders (D43).
//!
//! The daemon registers and connects sync roots through a
//! [`PlaceholderHost`]. The engine decides which mounts want one (local
//! mounts whose space has a `demand` rule) and, from the sync loop, keeps the
//! files on disk in line with the index: a placeholder without data for every
//! online-only file, downloaded ones converted to placeholders in sync.
//!
//! An online-only row whose stat is set has a placeholder on disk that Relay
//! put there. The scanner reads that file's absence as a delete; a row
//! without a stat is only in the index.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use relay_core::{EntryContent, EntryKey, EntryRecord, MountId, ObjectId, SpaceId, StatHint};
use relay_fs::cloud::{self, Probe};
use relay_fs::{ensure_real_dir_chain, to_os_path};

use crate::Engine;
use crate::error::EngineError;
use crate::materialize::{MaterializationMode, dehydrated_stat, path_mode};

/// A mount that wants online-only files as placeholders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaceholderRoot {
    pub space: SpaceId,
    pub mount: MountId,
    pub space_name: String,
    pub mount_name: String,
    pub path: PathBuf,
}

/// Registers and connects sync roots for the engine. Calls come from the sync
/// loop thread.
pub trait PlaceholderHost: Send + Sync {
    /// Mounts registered as sync roots, including by an earlier run.
    fn registered(&self) -> Vec<(MountId, PathBuf)>;
    /// Register and connect `root` unless it already is. False when this
    /// system, its volume, or the host's settings do not allow it.
    fn attach(&self, root: &PlaceholderRoot) -> bool;
    /// Disconnect and unregister. Placeholders without data are gone already.
    fn detach(&self, mount: MountId, path: &Path);
}

/// Placeholder state the engine keeps between loop passes.
#[derive(Default)]
pub(crate) struct Placeholders {
    host: Option<Arc<dyn PlaceholderHost>>,
    active: Vec<PlaceholderRoot>,
    dirty: HashSet<MountId>,
}

/// What one pass changed on disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaceholderReport {
    pub created: usize,
    pub converted: usize,
    pub updated: usize,
    pub removed: usize,
    pub failed: usize,
}

/// What a pass does for one row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// A placeholder without data for an online-only file.
    Create {
        object: ObjectId,
        size: u64,
        modified_ms: i64,
    },
    /// A downloaded plain file becomes a placeholder in sync.
    Convert { object: ObjectId },
    /// A placeholder without data for an older version: point it at this one.
    Refresh {
        object: ObjectId,
        size: u64,
        modified_ms: i64,
    },
    /// A downloaded placeholder whose bytes are this version again.
    MarkInSync {
        object: ObjectId,
        size: u64,
        modified_ms: i64,
    },
    /// The row's own placeholder: remember it is on disk.
    Placed(StatHint),
    /// The bytes were dropped outside Relay.
    Evicted(StatHint),
    /// Relay's placeholder for a path that is gone or no longer online-only.
    Remove,
    /// A deleted folder that only held placeholders.
    RemoveDir,
    /// An online-only folder.
    MakeDir,
}

/// The step that brings `disk` in line with `row`, if any.
pub(crate) fn plan_row(row: &EntryRecord, mode: MaterializationMode, disk: &Probe) -> Option<Step> {
    use MaterializationMode::{Demand, Exclude, Metadata};
    match (&row.content, mode) {
        (EntryContent::File { object, size, .. }, Demand) => {
            let (object, size, modified_ms) = (*object, *size, row.modified_at_unix_ms);
            match disk {
                Probe::Missing => (!row.materialized).then_some(Step::Create {
                    object,
                    size,
                    modified_ms,
                }),
                Probe::Placeholder {
                    stat,
                    object: found,
                    dehydrated: true,
                    ..
                } => {
                    if *found != Some(object) {
                        found.is_some().then_some(Step::Refresh {
                            object,
                            size,
                            modified_ms,
                        })
                    } else if row.materialized {
                        Some(Step::Evicted(*stat))
                    } else {
                        (row.stat != Some(*stat)).then_some(Step::Placed(*stat))
                    }
                }
                Probe::Placeholder {
                    stat,
                    object: found,
                    dehydrated: false,
                    in_sync,
                } => (row.materialized
                    && row.stat == Some(*stat)
                    && (*found != Some(object) || !in_sync))
                    .then_some(Step::MarkInSync {
                        object,
                        size,
                        modified_ms,
                    }),
                Probe::File(stat) => (row.materialized && row.stat == Some(*stat))
                    .then_some(Step::Convert { object }),
                Probe::Other => None,
            }
        }
        (EntryContent::File { .. }, Metadata | Exclude) => matches!(
            disk,
            Probe::Placeholder {
                dehydrated: true,
                object: Some(_),
                ..
            }
        )
        .then_some(Step::Remove),
        (EntryContent::Deleted, _) => match disk {
            Probe::Placeholder {
                dehydrated: true,
                object: Some(found),
                ..
            } if row.parent_object == Some(*found) => Some(Step::Remove),
            Probe::Other if mode == Demand => Some(Step::RemoveDir),
            _ => None,
        },
        (EntryContent::Directory, Demand) => {
            (!row.materialized && *disk == Probe::Missing).then_some(Step::MakeDir)
        }
        _ => None,
    }
}

impl Engine {
    /// Let `host` register sync roots for online-only mounts (D43).
    pub fn set_placeholder_host(&mut self, host: Arc<dyn PlaceholderHost>) {
        self.placeholders.host = Some(host);
    }

    /// Local mounts whose space has a `demand` rule.
    fn placeholder_roots_wanted(&self) -> Result<Vec<PlaceholderRoot>, EngineError> {
        let mut out = Vec::new();
        for (space, config) in self.mounts(None)? {
            let Some(path) = config.local_path else {
                continue;
            };
            let rules = self.db.repo().list_materialization_rules(space.id)?;
            if !rules
                .iter()
                .any(|rule| rule.mode == MaterializationMode::Demand.as_str())
            {
                continue;
            }
            out.push(PlaceholderRoot {
                space: space.id,
                mount: config.mount.id,
                space_name: space.name,
                mount_name: config.mount.name,
                path,
            });
        }
        Ok(out)
    }

    /// Attach the roots that are wanted and drop the rest, then queue a pass
    /// for every attached one. Runs at loop start and after config changes.
    pub(crate) fn refresh_placeholder_roots(&mut self) {
        let Some(host) = self.placeholders.host.clone() else {
            return;
        };
        let wanted = self.placeholder_roots_wanted().unwrap_or_else(|err| {
            tracing::warn!(error = %err, "could not list folders for online-only placeholders");
            Vec::new()
        });
        let active: Vec<PlaceholderRoot> = wanted.into_iter().filter(|r| host.attach(r)).collect();
        let is_active = |mount: MountId| active.iter().any(|root| root.mount == mount);
        for (mount, path) in host.registered() {
            if is_active(mount) {
                continue;
            }
            // Stop reading missing files as deletes before they go.
            if let Err(err) = self
                .db
                .transaction(|repo| repo.clear_unmaterialized_stats(mount))
            {
                tracing::warn!(%mount, error = %err, "could not stop tracking placeholders");
                continue;
            }
            let removed = cloud::remove_dehydrated(&path);
            tracing::info!(%mount, path = %path.display(), removed, "stopped online-only placeholders");
            host.detach(mount, &path);
        }
        if let Ok(mounts) = self.db.repo().list_mounts(None) {
            for config in mounts {
                if config.local_path.is_some() && !is_active(config.mount.id) {
                    let _ = self
                        .db
                        .transaction(|repo| repo.clear_unmaterialized_stats(config.mount.id));
                }
            }
        }
        self.placeholders.dirty = active.iter().map(|root| root.mount).collect();
        self.placeholders.active = active;
    }

    /// Queue a pass for the attached roots of `space`, or all of them.
    pub(crate) fn placeholders_changed(&mut self, space: Option<SpaceId>) {
        let Placeholders { active, dirty, .. } = &mut self.placeholders;
        for root in active.iter() {
            if space.is_none_or(|space| space == root.space) {
                dirty.insert(root.mount);
            }
        }
    }

    pub(crate) fn placeholders_pending(&self) -> bool {
        !self.placeholders.dirty.is_empty()
    }

    /// Run the queued passes.
    pub(crate) fn sync_placeholders(&mut self) {
        let dirty: Vec<MountId> = self.placeholders.dirty.drain().collect();
        for mount in dirty {
            let Some(root) = self
                .placeholders
                .active
                .iter()
                .find(|root| root.mount == mount)
                .cloned()
            else {
                continue;
            };
            match self.sync_placeholder_root(&root) {
                Ok(report) if report != PlaceholderReport::default() => {
                    tracing::debug!(mount = %root.mount_name, ?report, "placeholders updated");
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(mount = %root.mount_name, error = %err, "placeholder pass failed");
                }
            }
        }
    }

    fn sync_placeholder_root(
        &mut self,
        root: &PlaceholderRoot,
    ) -> Result<PlaceholderReport, EngineError> {
        let rules = self.db.repo().list_materialization_rules(root.space)?;
        let mut rows = self.db.repo().entries_for_mount(root.mount)?;
        rows.sort_by(|a, b| a.key.path.cmp(&b.key.path));
        let mut steps = Vec::new();
        for row in rows {
            let mode = path_mode(&rules, &root.mount_name, row.key.path.as_str())?;
            let Ok(dest) = to_os_path(&root.path, &row.key.path) else {
                continue;
            };
            // The common case, an online-only file whose placeholder is in
            // place, needs one stat and no handle.
            if mode == MaterializationMode::Demand
                && !row.materialized
                && row.stat.is_some()
                && matches!(row.content, EntryContent::File { .. })
                && dehydrated_stat(&dest) == row.stat
            {
                continue;
            }
            let disk = match cloud::probe(&dest) {
                Ok(disk) => disk,
                Err(_) => continue,
            };
            if let Some(step) = plan_row(&row, mode, &disk) {
                steps.push((row.key, dest, step));
            }
        }

        let mut report = PlaceholderReport::default();
        let mut stats: Vec<(EntryKey, Option<StatHint>)> = Vec::new();
        // Removals deepest first, so a folder is empty by the time it goes.
        for (_, dest, step) in steps.iter().rev() {
            match step {
                Step::Remove => match fs::remove_file(dest) {
                    Ok(()) => report.removed += 1,
                    Err(_) => report.failed += 1,
                },
                Step::RemoveDir => {
                    let empty_dir = fs::symlink_metadata(dest)
                        .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink());
                    if empty_dir && fs::remove_dir(dest).is_ok() {
                        report.removed += 1;
                    }
                }
                _ => {}
            }
        }
        for (key, dest, step) in steps {
            let done = match step {
                Step::Remove | Step::RemoveDir => continue,
                Step::MakeDir => self.materialize_indexed(&key).map(|()| None),
                Step::Placed(stat) | Step::Evicted(stat) => Ok(Some(stat)),
                Step::Create {
                    object,
                    size,
                    modified_ms,
                } => dest
                    .parent()
                    .map_or(Ok(()), |parent| ensure_real_dir_chain(&root.path, parent))
                    .and_then(|()| cloud::create(&dest, object, size, modified_ms))
                    .map_err(EngineError::from)
                    .map(|()| {
                        report.created += 1;
                        dehydrated_stat(&dest)
                    }),
                Step::Refresh {
                    object,
                    size,
                    modified_ms,
                } => cloud::update(&dest, object, size, modified_ms, true)
                    .map_err(EngineError::from)
                    .map(|()| {
                        report.updated += 1;
                        dehydrated_stat(&dest)
                    }),
                Step::MarkInSync {
                    object,
                    size,
                    modified_ms,
                } => cloud::update(&dest, object, size, modified_ms, false)
                    .map_err(EngineError::from)
                    .map(|()| {
                        report.updated += 1;
                        None
                    }),
                Step::Convert { object } => cloud::convert(&dest, object)
                    .map_err(EngineError::from)
                    .map(|()| {
                        report.converted += 1;
                        None
                    }),
            };
            match done {
                // Index-only, with the placeholder's stat as the marker.
                Ok(Some(stat)) => stats.push((key, Some(stat))),
                Ok(None) => {}
                Err(err) => {
                    report.failed += 1;
                    tracing::debug!(path = %dest.display(), error = %err, "placeholder step failed");
                }
            }
        }
        if !stats.is_empty() {
            self.db
                .transaction(|repo| {
                    for (key, stat) in &stats {
                        repo.set_materialized(key, false, *stat)?;
                    }
                    Ok::<(), relay_db::DbError>(())
                })
                .map_err(EngineError::from_db)?;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use relay_core::{DeviceId, LogicalPath, Sequence, VersionVector};

    fn row(content: EntryContent, materialized: bool, stat: Option<StatHint>) -> EntryRecord {
        EntryRecord {
            key: EntryKey {
                space: SpaceId::new(),
                mount: MountId::new(),
                path: LogicalPath::new("a.txt").unwrap(),
            },
            content,
            vector: VersionVector::default(),
            parent_object: None,
            sequence: Sequence(1),
            modified_by: DeviceId::random(),
            modified_at_unix_ms: 1_000,
            stat,
            materialized,
        }
    }

    fn stat(size: u64) -> StatHint {
        StatHint {
            size,
            mtime_ns: 7,
            file_id: None,
            ctime_ns: None,
        }
    }

    fn file(object: ObjectId) -> EntryContent {
        EntryContent::File {
            object,
            size: 3,
            executable: false,
        }
    }

    fn placeholder(object: Option<ObjectId>, dehydrated: bool, in_sync: bool) -> Probe {
        Probe::Placeholder {
            stat: stat(3),
            object,
            dehydrated,
            in_sync,
        }
    }

    const DEMAND: MaterializationMode = MaterializationMode::Demand;

    #[test]
    fn online_only_files_get_placeholders() {
        let a = ObjectId::of(b"abc");
        let online = row(file(a), false, None);
        assert_eq!(
            plan_row(&online, DEMAND, &Probe::Missing),
            Some(Step::Create {
                object: a,
                size: 3,
                modified_ms: 1_000
            })
        );
        // A downloaded file that went missing is the scanner's delete.
        let downloaded = row(file(a), true, Some(stat(3)));
        assert_eq!(plan_row(&downloaded, DEMAND, &Probe::Missing), None);
        // Full mode writes real files; no placeholders.
        assert_eq!(
            plan_row(&online, MaterializationMode::Full, &Probe::Missing),
            None
        );
    }

    #[test]
    fn placeholders_without_data_track_the_row() {
        let a = ObjectId::of(b"abc");
        let b = ObjectId::of(b"newer");
        let online = row(file(a), false, None);
        assert_eq!(
            plan_row(&online, DEMAND, &placeholder(Some(a), true, true)),
            Some(Step::Placed(stat(3)))
        );
        let placed = row(file(a), false, Some(stat(3)));
        assert_eq!(
            plan_row(&placed, DEMAND, &placeholder(Some(a), true, true)),
            None
        );
        let freed = row(file(a), true, Some(stat(9)));
        assert_eq!(
            plan_row(&freed, DEMAND, &placeholder(Some(a), true, true)),
            Some(Step::Evicted(stat(3)))
        );
        let updated_remotely = row(file(b), false, None);
        assert!(matches!(
            plan_row(&updated_remotely, DEMAND, &placeholder(Some(a), true, true)),
            Some(Step::Refresh { object, .. }) if object == b
        ));
        // Someone else's placeholder is left alone.
        assert_eq!(
            plan_row(&updated_remotely, DEMAND, &placeholder(None, true, false)),
            None
        );
    }

    #[test]
    fn downloaded_files_become_placeholders_in_sync() {
        let a = ObjectId::of(b"abc");
        let b = ObjectId::of(b"edited");
        let downloaded = row(file(a), true, Some(stat(3)));
        assert_eq!(
            plan_row(&downloaded, DEMAND, &Probe::File(stat(3))),
            Some(Step::Convert { object: a })
        );
        // Changed since it was indexed: wait for the scanner.
        assert_eq!(plan_row(&downloaded, DEMAND, &Probe::File(stat(4))), None);
        assert_eq!(
            plan_row(&downloaded, DEMAND, &placeholder(Some(a), false, true)),
            None
        );
        let edited = row(file(b), true, Some(stat(3)));
        assert!(matches!(
            plan_row(&edited, DEMAND, &placeholder(Some(a), false, false)),
            Some(Step::MarkInSync { object, .. }) if object == b
        ));
    }

    #[test]
    fn leftovers_of_deleted_rows_go() {
        let a = ObjectId::of(b"abc");
        let mut deleted = row(EntryContent::Deleted, true, None);
        deleted.parent_object = Some(a);
        assert_eq!(
            plan_row(&deleted, DEMAND, &placeholder(Some(a), true, true)),
            Some(Step::Remove)
        );
        // A different file moved onto a deleted path is the scanner's create.
        let other = ObjectId::of(b"other");
        assert_eq!(
            plan_row(&deleted, DEMAND, &placeholder(Some(other), true, true)),
            None
        );
        assert_eq!(
            plan_row(&deleted, DEMAND, &Probe::Other),
            Some(Step::RemoveDir)
        );
        let excluded = row(file(a), false, None);
        assert_eq!(
            plan_row(
                &excluded,
                MaterializationMode::Exclude,
                &placeholder(Some(a), true, true)
            ),
            Some(Step::Remove)
        );
    }

    #[derive(Default)]
    struct FakeHost {
        allow: bool,
        registered: Mutex<Vec<(MountId, PathBuf)>>,
        attached: Mutex<Vec<MountId>>,
        detached: Mutex<Vec<MountId>>,
    }

    impl PlaceholderHost for FakeHost {
        fn registered(&self) -> Vec<(MountId, PathBuf)> {
            self.registered.lock().unwrap().clone()
        }
        fn attach(&self, root: &PlaceholderRoot) -> bool {
            self.attached.lock().unwrap().push(root.mount);
            self.allow
        }
        fn detach(&self, mount: MountId, _path: &Path) {
            self.detached.lock().unwrap().push(mount);
            self.registered.lock().unwrap().retain(|(m, _)| *m != mount);
        }
    }

    fn engine_with_file() -> (tempfile::TempDir, tempfile::TempDir, Engine, EntryRecord) {
        let home = tempfile::TempDir::new().unwrap();
        let mount = tempfile::TempDir::new().unwrap();
        let mut engine = Engine::init(home.path(), "testdev").unwrap();
        engine.create_space("Personal").unwrap();
        engine
            .add_mount("Personal", "code", mount.path(), &[], &[])
            .unwrap();
        fs::write(mount.path().join("a.txt"), b"abc").unwrap();
        engine
            .scan("Personal", "code", crate::ScanOptions::default())
            .unwrap();
        let row = engine
            .entries("Personal", "code", false)
            .unwrap()
            .into_iter()
            .find(|e| e.key.path.as_str() == "a.txt")
            .unwrap();
        (home, mount, engine, row)
    }

    #[test]
    fn only_mounts_with_online_only_rules_become_roots() {
        let (_home, _mount, mut engine, row) = engine_with_file();
        let host = Arc::new(FakeHost {
            allow: true,
            ..FakeHost::default()
        });
        engine.set_placeholder_host(host.clone());
        engine.refresh_placeholder_roots();
        assert!(host.attached.lock().unwrap().is_empty());
        assert!(!engine.placeholders_pending());

        engine
            .materialize_add("Personal", "online", "demand", &["code/**".into()])
            .unwrap();
        engine.refresh_placeholder_roots();
        assert_eq!(*host.attached.lock().unwrap(), vec![row.key.mount]);
        assert!(engine.placeholders_pending());
        // Off Windows every step fails; the pass must not.
        engine.sync_placeholders();
        assert!(!engine.placeholders_pending());
    }

    #[test]
    fn dropping_a_root_stops_reading_missing_files_as_deletes() {
        let (_home, mount, mut engine, row) = engine_with_file();
        engine
            .db
            .transaction(|repo| repo.set_materialized(&row.key, false, row.stat))
            .unwrap();
        // Registered by an earlier run; the host now refuses it.
        let host = Arc::new(FakeHost::default());
        host.registered
            .lock()
            .unwrap()
            .push((row.key.mount, mount.path().to_path_buf()));
        engine
            .materialize_add("Personal", "online", "demand", &["code/**".into()])
            .unwrap();
        engine.set_placeholder_host(host.clone());
        engine.refresh_placeholder_roots();
        assert_eq!(*host.detached.lock().unwrap(), vec![row.key.mount]);
        let after = engine.db.repo().entry(&row.key).unwrap().unwrap();
        assert!(!after.materialized);
        assert_eq!(after.stat, None);
        // Plain files are not placeholders and stay.
        assert!(mount.path().join("a.txt").exists());
    }

    #[test]
    fn online_only_folders_exist_on_disk() {
        let dir = row(EntryContent::Directory, false, None);
        assert_eq!(plan_row(&dir, DEMAND, &Probe::Missing), Some(Step::MakeDir));
        assert_eq!(plan_row(&dir, DEMAND, &Probe::Other), None);
    }
}
