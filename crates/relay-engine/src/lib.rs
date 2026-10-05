//! Integration layer: local index, object store, and filesystem scans.

mod apply;
mod clock;
mod config;
mod error;
mod files;
mod live_config;
mod materialize;
mod order;
mod peers;
mod placeholders;
mod policies;
mod progress;
mod replica;
mod reports;
mod resolve;
mod scan;
mod secrets;
mod sync;
mod watch;

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use relay_core::{DeviceId, EntryKey, MOUNT_MARKER, validate_name};
use relay_crypto::BoxKeyPair;
use relay_db::Database;
use relay_fs::{MaterializeOptions, MountMarker, materialize_file, resolve_os_path, to_os_path};
use relay_policy::MountRules;
use relay_store::StoreError;

pub use clock::{Clock, ManualClock, SystemClock};
pub use error::EngineError;
pub use files::{CopyState, FileRow, FolderView};
pub use materialize::{MaterializationInfo, MaterializationMode};
pub use peers::{
    AdoptedMembers, ConflictClass, ConflictInfo, OfferInfo, PeerInfo, classify_conflict,
    group_git_conflicts,
};
pub use placeholders::{PlaceholderHost, PlaceholderReport, PlaceholderRoot};
pub use policies::{GroupInfo, PolicyInfo};
pub use progress::{
    Bookend, TransferDirection, TransferLive, bookends, format_bytes, format_rate, index_row,
    summary_line,
};
pub use relay_core::{
    ConfigApplied, ConfigChange, DeleteHoldDecision, Device, EntryContent, EntryKind, EntryRecord,
    LogicalPath, Mount, MountId, ObjectId, Sequence, Space, SpaceId, VectorOrdering,
};
pub use relay_crypto::DeviceIdentity;
pub use relay_db::{HistoryRecord, MountConfig};
pub use relay_fs::{FsError, ScanWarning};
pub use relay_store::ObjectStore;
pub use replica::{ReplicaPull, ReplicaPush, ReplicaSpacePush, ReplicaStatus, TransportStatus};
pub use reports::{
    DeleteHold, GcReport, MASS_DELETE_DENOMINATOR, MASS_DELETE_MIN_COUNT, MASS_DELETE_NUMERATOR,
    MountStatus, PeerSpaceStatus, PeerStatus, ScanOptions, ScanReport, Status, VerifyReport,
    Warning,
};
pub use resolve::{
    GitResolveReport, Resolution, ResolveReport, resolve_conflict, resolve_git_conflicts,
};
pub use sync::{PairedPeer, Rejected, SyncEvent, SyncInput, SyncOutput, Syncer};
pub use watch::{RunExit, WatchEvent, WatchOptions};

const DB_FILE: &str = "relay.db";
const STORE_DIR: &str = "store";
const LOGS_DIR: &str = "logs";
const IDENTITY_DIR: &str = "identity";
const LOCK_FILE: &str = "relay.lock";
const RUN_LOCK_FILE: &str = "relay.run.lock";
const TMP_CLEAN_AGE: Duration = Duration::from_secs(60 * 60);
const SCHEMA_UPGRADE_WAIT: Duration = Duration::from_secs(5);

/// Engine knobs that are not part of the persisted device identity.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// If a file's mtime is newer than this, its stat hint is stored as `None`
    /// so the next scan re-hashes (Git's "racy clean" case).
    pub racy_window: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            racy_window: Duration::from_secs(2),
        }
    }
}

/// `$RELAY_HOME` if set; else the platform project data dir; else `~/.relay`.
pub fn default_home() -> PathBuf {
    if let Ok(home) = std::env::var("RELAY_HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home);
    }
    if let Some(dirs) = directories::ProjectDirs::from("dev", "Relay", "Relay") {
        return dirs.data_dir().to_path_buf();
    }
    fallback_dot_relay()
}

fn fallback_dot_relay() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(home).join(".relay");
    }
    PathBuf::from(".relay")
}

pub struct Engine {
    home: PathBuf,
    db: Database,
    store: ObjectStore,
    device: Device,
    clock: Arc<dyn Clock>,
    config: EngineConfig,
    /// X25519 key that unwraps space keys. `None` only for a read-only open of
    /// a home that has not generated one yet.
    box_key: Option<BoxKeyPair>,
    /// Exclusive lock on `<home>/relay.lock`. `None` for a read-only engine.
    lock: Option<File>,
    /// Sync roots for online-only files (D43). Empty unless a host is set.
    placeholders: placeholders::Placeholders,
}

impl Engine {
    pub fn init(home: &Path, device_name: &str) -> Result<Engine, EngineError> {
        validate_name(device_name)?;
        ensure_layout(home)?;
        let lock = acquire_lock(home)?;
        // Key first, then the DB row: a crash cannot leave a database with no
        // identity. If the key exists from a previous interrupted init, reuse it.
        let identity = load_or_generate_identity(&home.join(IDENTITY_DIR))?;
        let mut db = Database::open(&home.join(DB_FILE))?;
        if db.repo().local_device()?.is_some() {
            return Err(EngineError::AlreadyInitialized);
        }
        let device = Device {
            id: identity.device_id(),
            name: device_name.to_owned(),
        };
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let now = clock.now_ms();
        db.transaction(|repo| repo.init_local_device(&device, now))
            .map_err(EngineError::from_db)?;
        let box_key = BoxKeyPair::load_or_generate(&home.join(IDENTITY_DIR))?;
        let store = ObjectStore::open(home.join(STORE_DIR))?;
        Ok(Engine {
            home: home.to_path_buf(),
            db,
            store,
            device,
            clock,
            config: EngineConfig::default(),
            box_key: Some(box_key),
            lock: Some(lock),
            placeholders: Default::default(),
        })
    }

    /// Open the home for writing. Fails with [`EngineError::Running`] while a
    /// sync loop ([`Engine::run`]) owns the home: scans, restores and GC write
    /// entries the loop is also writing, and two writers could hand out the
    /// same version counter for different content.
    pub fn open(home: &Path) -> Result<Engine, EngineError> {
        Self::open_inner(home, true)
    }

    /// Open the home to change configuration (peers, shares, spaces, mounts)
    /// even while a sync loop runs. The loop notices the commit and reloads.
    /// Do not use this handle to scan, restore or collect garbage.
    pub fn open_for_config(home: &Path) -> Result<Engine, EngineError> {
        Self::open_inner(home, false)
    }

    fn open_inner(home: &Path, exclusive: bool) -> Result<Engine, EngineError> {
        let db_path = home.join(DB_FILE);
        if !db_path.is_file() {
            return Err(EngineError::NotInitialized);
        }
        ensure_layout(home)?;
        let lock = acquire_lock(home)?;
        if exclusive && run_lock_held(home)? {
            return Err(EngineError::Running {
                home: home.to_path_buf(),
            });
        }
        let db = Database::open(&db_path)?;
        let local = db
            .repo()
            .local_device()
            .map_err(EngineError::from_db)?
            .ok_or(EngineError::NotInitialized)?;
        let identity = require_matching_identity(home, local.device.id)?;
        let _ = identity;
        let box_key = BoxKeyPair::load_or_generate(&home.join(IDENTITY_DIR))?;
        let store = ObjectStore::open(home.join(STORE_DIR))?;
        Ok(Engine {
            home: home.to_path_buf(),
            db,
            store,
            device: local.device,
            clock: Arc::new(SystemClock),
            config: EngineConfig::default(),
            box_key: Some(box_key),
            lock: Some(lock),
            placeholders: Default::default(),
        })
    }

    /// Open the home for reads only. Does not take the writer lock.
    pub fn open_read_only(home: &Path) -> Result<Engine, EngineError> {
        let db_path = home.join(DB_FILE);
        if !db_path.is_file() {
            return Err(EngineError::NotInitialized);
        }
        let db = open_read_only_db(home, &db_path)?;
        let local = db
            .repo()
            .local_device()
            .map_err(EngineError::from_db)?
            .ok_or(EngineError::NotInitialized)?;
        let identity = require_matching_identity(home, local.device.id)?;
        let _ = identity;
        let box_key = BoxKeyPair::load(&home.join(IDENTITY_DIR)).ok();
        let store = ObjectStore::open(home.join(STORE_DIR))?;
        Ok(Engine {
            home: home.to_path_buf(),
            db,
            store,
            device: local.device,
            clock: Arc::new(SystemClock),
            config: EngineConfig::default(),
            box_key,
            lock: None,
            placeholders: Default::default(),
        })
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Engine {
        self.clock = clock;
        self
    }

    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }

    pub fn with_config(mut self, config: EngineConfig) -> Engine {
        self.config = config;
        self
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    pub fn identity_dir(&self) -> PathBuf {
        self.home.join(IDENTITY_DIR)
    }

    pub fn load_identity(&self) -> Result<DeviceIdentity, EngineError> {
        DeviceIdentity::load(&self.identity_dir()).map_err(|err| match err {
            relay_crypto::CryptoError::Io { .. } => EngineError::StaleIdentity {
                home: self.home.clone(),
            },
            other => EngineError::Crypto(other),
        })
    }

    pub fn store(&self) -> &ObjectStore {
        &self.store
    }

    /// SQLite `PRAGMA data_version` on the engine's write connection.
    ///
    /// Increments only when another connection commits. Every engine write
    /// (scans, sync apply, config) goes through [`Database`]'s single `conn`.
    pub fn data_version(&self) -> Result<u32, EngineError> {
        Ok(self.db.data_version()?)
    }

    pub fn paused(&self) -> Result<bool, EngineError> {
        Ok(self.db.repo().local_setting("paused")?.as_deref() == Some("1"))
    }

    pub fn set_paused(&mut self, paused: bool) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let value = if paused { "1" } else { "0" };
        self.db
            .transaction(|repo| repo.set_local_setting("paused", value))
            .map_err(EngineError::from_db)
    }

    pub fn create_space(&mut self, name: &str) -> Result<Space, EngineError> {
        self.ensure_writable()?;
        validate_name(name)?;
        let space = Space {
            id: relay_core::SpaceId::new(),
            name: name.to_owned(),
        };
        let now = self.clock.now_ms();
        self.db
            .transaction(|repo| repo.create_space(&space, now))
            .map_err(EngineError::from_db)?;
        self.ensure_space_key(space.id)?;
        Ok(space)
    }

    pub fn spaces(&self) -> Result<Vec<Space>, EngineError> {
        Ok(self.db.repo().list_spaces()?)
    }

    pub fn add_mount(
        &mut self,
        space: &str,
        mount: &str,
        local_path: &Path,
        includes: &[String],
        excludes: &[String],
    ) -> Result<MountConfig, EngineError> {
        self.ensure_writable()?;
        validate_name(space)?;
        validate_name(mount)?;
        let meta = fs::metadata(local_path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                EngineError::PathNotADirectory(local_path.to_path_buf())
            } else {
                EngineError::Io(err)
            }
        })?;
        if !meta.is_dir() {
            return Err(EngineError::PathNotADirectory(local_path.to_path_buf()));
        }

        let canonical = dunce::canonicalize(local_path)?;
        let home = dunce::canonicalize(&self.home).unwrap_or_else(|_| self.home.clone());
        if paths_overlap(&canonical, &home) {
            return Err(EngineError::OverlapsRelayHome);
        }

        let existing = self.db.repo().list_mounts(None)?;
        for cfg in &existing {
            let Some(other) = cfg.local_path.as_ref() else {
                continue;
            };
            let other = dunce::canonicalize(other).unwrap_or_else(|_| other.clone());
            if paths_overlap(&canonical, &other) {
                return Err(EngineError::OverlappingMount { existing: other });
            }
        }

        if canonical.join(MOUNT_MARKER).exists() {
            if !self.can_adopt_leftover_marker(&canonical)? {
                return Err(EngineError::MountAlreadyClaimed { path: canonical });
            }
            tracing::info!(
                path = %canonical.display(),
                "adopting leftover mount marker written by this device"
            );
        }

        MountRules::new(includes, excludes)?;

        let space_rec = self
            .db
            .repo()
            .space_by_name(space)?
            .ok_or_else(|| EngineError::UnknownSpace(space.to_owned()))?;

        if let Some(existing) = self.db.repo().mount_by_name(space_rec.id, mount)? {
            let config = self
                .db
                .repo()
                .mount_config(existing.id)?
                .ok_or(EngineError::MountNotLocal)?;
            if config.local_path.is_some() {
                return Err(EngineError::MountAlreadyAttached {
                    space: space.to_owned(),
                    mount: mount.to_owned(),
                });
            }
            return self.attach_mount(&space_rec, existing, &canonical, includes, excludes);
        }

        let mount_rec = Mount {
            id: relay_core::MountId::new(),
            space: space_rec.id,
            name: mount.to_owned(),
        };

        let marker = MountMarker {
            space: space_rec.id,
            mount: mount_rec.id,
            created_by: self.device.id,
        };
        marker.write(&canonical)?;

        let now = self.clock.now_ms();
        let db_result = self.db.transaction(|repo| {
            repo.create_mount(&mount_rec, now)?;
            repo.set_local_mount_path(mount_rec.id, &canonical)?;
            repo.set_mount_rules(mount_rec.id, includes, excludes)?;
            Ok::<(), EngineError>(())
        });
        if let Err(err) = db_result {
            let _ = fs::remove_file(canonical.join(MOUNT_MARKER));
            return Err(match err {
                EngineError::Db(inner) => EngineError::from_db(inner),
                other => other,
            });
        }

        self.db
            .repo()
            .mount_config(mount_rec.id)?
            .ok_or(EngineError::MountNotLocal)
    }

    fn attach_mount(
        &mut self,
        space: &Space,
        mount: Mount,
        canonical: &Path,
        includes: &[String],
        excludes: &[String],
    ) -> Result<MountConfig, EngineError> {
        let marker = MountMarker {
            space: space.id,
            mount: mount.id,
            created_by: self.device.id,
        };
        marker.write(canonical)?;
        let db_result = self.db.transaction(|repo| {
            repo.set_local_mount_path(mount.id, canonical)?;
            repo.set_mount_rules(mount.id, includes, excludes)?;
            repo.reset_received_seq_for_space(space.id)?;
            Ok::<(), EngineError>(())
        });
        if let Err(err) = db_result {
            let _ = fs::remove_file(canonical.join(MOUNT_MARKER));
            return Err(match err {
                EngineError::Db(inner) => EngineError::from_db(inner),
                other => other,
            });
        }
        self.db
            .repo()
            .mount_config(mount.id)?
            .ok_or(EngineError::MountNotLocal)
    }

    pub fn mounts(&self, space: Option<&str>) -> Result<Vec<(Space, MountConfig)>, EngineError> {
        let space_id = match space {
            Some(name) => Some(
                self.db
                    .repo()
                    .space_by_name(name)?
                    .ok_or_else(|| EngineError::UnknownSpace(name.to_owned()))?
                    .id,
            ),
            None => None,
        };
        let configs = self.db.repo().list_mounts(space_id)?;
        let mut out = Vec::with_capacity(configs.len());
        for config in configs {
            let space = self
                .db
                .repo()
                .space(config.mount.space)?
                .ok_or_else(|| EngineError::UnknownSpace(config.mount.name.clone()))?;
            out.push((space, config));
        }
        Ok(out)
    }

    /// Resolve SPACE[/MOUNT] to local mount ids for a forced rescan.
    pub fn resolve_rescan_targets(
        &self,
        space: Option<&str>,
        mount: Option<&str>,
    ) -> Result<Vec<(SpaceId, MountId, String)>, EngineError> {
        let listed = self.mounts(space)?;
        let mut out = Vec::new();
        for (space_rec, config) in listed {
            if let Some(name) = mount
                && config.mount.name != name
            {
                continue;
            }
            if config.local_path.is_none() {
                continue;
            }
            out.push((
                space_rec.id,
                config.mount.id,
                format!("{}/{}", space_rec.name, config.mount.name),
            ));
        }
        Ok(out)
    }

    pub fn scan(
        &mut self,
        space: &str,
        mount: &str,
        opts: ScanOptions,
    ) -> Result<ScanReport, EngineError> {
        self.scan_mount(space, mount, opts, &mut |_| {})
    }

    /// Incremental scan of `paths` (and the scopes `scan_paths` derives).
    pub fn scan_paths(
        &mut self,
        space: &str,
        mount: &str,
        paths: &[LogicalPath],
        opts: ScanOptions,
    ) -> Result<ScanReport, EngineError> {
        self.scan_paths_inner(space, mount, paths, opts, &mut |_| {})
    }

    #[allow(clippy::type_complexity)]
    pub fn scan_all(
        &mut self,
        opts: ScanOptions,
    ) -> Result<Vec<(Space, Mount, Result<ScanReport, EngineError>)>, EngineError> {
        let listed = self.mounts(None)?;
        let mut out = Vec::with_capacity(listed.len());
        for (space, config) in listed {
            let mount = config.mount.clone();
            let report = self.scan(&space.name, &mount.name, opts);
            out.push((space, mount, report));
        }
        Ok(out)
    }

    pub fn entries(
        &self,
        space: &str,
        mount: &str,
        include_deleted: bool,
    ) -> Result<Vec<EntryRecord>, EngineError> {
        let (_, config) = self.lookup_mount(space, mount)?;
        let mut entries = self.db.repo().entries_for_mount(config.mount.id)?;
        if !include_deleted {
            entries.retain(|e| !e.is_deleted());
        }
        Ok(entries)
    }

    pub fn history(
        &self,
        space: &str,
        mount: &str,
        path: &LogicalPath,
    ) -> Result<Vec<HistoryRecord>, EngineError> {
        let (space_rec, config) = self.lookup_mount(space, mount)?;
        let key = EntryKey {
            space: space_rec.id,
            mount: config.mount.id,
            path: path.clone(),
        };
        Ok(self.db.repo().history(&key)?)
    }

    pub fn restore(
        &mut self,
        space: &str,
        mount: &str,
        path: &LogicalPath,
        sequence: Sequence,
    ) -> Result<EntryRecord, EngineError> {
        self.ensure_writable()?;
        let (space_rec, config) = self.lookup_mount(space, mount)?;
        let local_path = config
            .local_path
            .clone()
            .ok_or(EngineError::MountNotLocal)?;
        relay_fs::MountMarker::verify(&local_path, config.mount.id)?;

        let key = EntryKey {
            space: space_rec.id,
            mount: config.mount.id,
            path: path.clone(),
        };
        let history = self.db.repo().history(&key)?;
        let version = history
            .iter()
            .find(|h| h.sequence == sequence)
            .ok_or(EngineError::UnknownVersion)?;
        let (object, size, executable) = match &version.content {
            EntryContent::File {
                object,
                size,
                executable,
            } => (*object, *size, *executable),
            other => {
                return Err(EngineError::RestoreUnsupported(format!(
                    "only files can be restored (found {})",
                    content_kind_name(other)
                )));
            }
        };
        let current = self.db.repo().entry(&key)?;
        self.restore_file_version(
            &key,
            &local_path,
            object,
            size,
            executable,
            current.as_ref(),
        )
    }

    pub fn verify_objects(&self) -> Result<VerifyReport, EngineError> {
        let live = self.db.repo().verification_objects()?;
        let mut missing = Vec::new();
        let mut corrupt = Vec::new();
        for id in &live {
            match self.store.verify(id) {
                Ok(()) => {}
                Err(StoreError::NotFound(_)) => missing.push(*id),
                Err(StoreError::Corrupt { .. }) => corrupt.push(*id),
                Err(err) => return Err(err.into()),
            }
        }
        missing.sort();
        corrupt.sort();
        Ok(VerifyReport {
            checked: live.len(),
            missing,
            corrupt,
        })
    }

    pub fn gc(&mut self, grace: Duration) -> Result<GcReport, EngineError> {
        self.ensure_writable()?;
        let live = self.db.repo().live_objects()?;
        let sweep = self.store.sweep(&live, grace)?;
        let tmp_cleaned = self.store.clean_tmp(TMP_CLEAN_AGE)?;
        Ok(GcReport {
            removed: sweep.removed,
            bytes_freed: sweep.bytes_freed,
            kept: sweep.kept,
            tmp_cleaned,
        })
    }

    pub fn delete_holds(&self) -> Result<Vec<DeleteHold>, EngineError> {
        Ok(self
            .db
            .repo()
            .list_delete_holds()?
            .into_iter()
            .map(|row| DeleteHold {
                peer: row.peer.id,
                peer_name: row.peer.name,
                space: row.space.name,
                space_id: row.space.id,
                mount: row.mount.name,
                mount_id: row.mount.id,
                deletions: row.deletions,
                live: row.live,
                held_at_ms: row.held_at_ms,
                decision: row.decision,
            })
            .collect())
    }

    pub fn decide_delete_hold(
        &mut self,
        space: &str,
        mount: Option<&str>,
        peer: Option<&str>,
        decision: DeleteHoldDecision,
    ) -> Result<usize, EngineError> {
        self.ensure_writable()?;
        let space_rec = self
            .db
            .repo()
            .space_by_name(space)?
            .ok_or_else(|| EngineError::UnknownSpace(space.to_owned()))?;
        let mount_id = match mount {
            Some(name) => Some(
                self.db
                    .repo()
                    .mount_by_name(space_rec.id, name)?
                    .ok_or_else(|| EngineError::UnknownMount {
                        space: space.to_owned(),
                        mount: name.to_owned(),
                    })?
                    .id,
            ),
            None => None,
        };
        let peer_id = match peer {
            Some(name) => Some(
                self.find_peer(name)?
                    .ok_or_else(|| EngineError::UnknownPeer(name.to_owned()))?
                    .device
                    .id,
            ),
            None => None,
        };
        let now = self.clock.now_ms();
        self.db
            .transaction(|repo| {
                repo.decide_delete_holds(space_rec.id, mount_id, peer_id, decision, now)
            })
            .map_err(EngineError::from_db)
    }

    /// Re-issue a local version of each still-matching live entry so a peer
    /// tombstone becomes concurrent with a live winner (D18 rule 1).
    pub(crate) fn reassert_live(&mut self, keys: &[EntryKey]) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let now = self.clock.now_ms();
        let device = self.device.id;
        let mut writes = Vec::new();
        for key in keys {
            let Some(current) = self.db.repo().entry(key)? else {
                continue;
            };
            if current.is_deleted() {
                continue;
            }
            let Some(config) = self.db.repo().mount_config(key.mount)? else {
                continue;
            };
            let Some(root) = config.local_path.as_ref() else {
                continue;
            };
            if !live_entry_matches_disk(&self.store, root, &current)? {
                continue;
            }
            writes.push(current);
        }
        if writes.is_empty() {
            return Ok(());
        }
        self.db
            .transaction(|repo| {
                for prev in &writes {
                    let sequence = repo.next_sequence()?;
                    let record = EntryRecord::local_write(
                        Some(prev),
                        prev.key.clone(),
                        prev.content.clone(),
                        prev.stat,
                        device,
                        now,
                        sequence,
                    );
                    repo.put_entry(&record)?;
                }
                Ok::<(), EngineError>(())
            })
            .map_err(|err| match err {
                EngineError::Db(inner) => EngineError::from_db(inner),
                other => other,
            })
    }

    /// Write a historical file version to disk and record a new local version
    /// whose vector dominates `current` (the tombstone, for a recreate).
    fn restore_file_version(
        &mut self,
        key: &EntryKey,
        local_path: &Path,
        object: ObjectId,
        size: u64,
        executable: bool,
        current: Option<&EntryRecord>,
    ) -> Result<EntryRecord, EngineError> {
        let dest = match resolve_os_path(local_path, &key.path)? {
            Some(existing) => existing,
            None => to_os_path(local_path, &key.path)?,
        };
        let expected_existing = restore_expected_stat(&self.store, &dest, current)?;

        let mut reader = self.store.open_object(&object)?;
        let stat = match materialize_file(
            &mut reader,
            &dest,
            object,
            executable,
            expected_existing.as_ref(),
            MaterializeOptions {
                mount_root: local_path,
                mtime_ns: None,
            },
        ) {
            Ok(stat) => stat,
            Err(relay_fs::FsError::DestinationChanged(path)) => {
                return Err(EngineError::DestinationChanged(path));
            }
            Err(err) => return Err(err.into()),
        };
        let stat = scan::recorded_stat(stat, scan::wall_clock_now_ns(), self.config.racy_window);

        let now = self.clock.now_ms();
        let device = self.device.id;
        let content = EntryContent::File {
            object,
            size,
            executable,
        };
        self.db
            .transaction(|repo| {
                repo.record_object(object, size, now)?;
                let sequence = repo.next_sequence()?;
                let record = EntryRecord::local_write(
                    current,
                    key.clone(),
                    content.clone(),
                    stat,
                    device,
                    now,
                    sequence,
                );
                repo.put_entry(&record)?;
                Ok(record)
            })
            .map_err(|err| match err {
                EngineError::Db(inner) => EngineError::from_db(inner),
                other => other,
            })
    }

    /// Recreate files deleted by this peer's already-applied tombstones.
    ///
    /// Skips a path when a precondition fails. Other per-path errors are
    /// returned as warnings so the caller can emit `SyncWarning`.
    pub(crate) fn resurrect_applied_deletes(
        &mut self,
        peer: DeviceId,
        keys: &[EntryKey],
    ) -> Result<Vec<(String, String)>, EngineError> {
        self.ensure_writable()?;
        let mut warnings = Vec::new();
        for key in keys {
            match self.try_resurrect_applied(peer, key) {
                Ok(()) => {}
                Err(ResurrectSkip::Precondition) => {}
                Err(ResurrectSkip::Warn(reason)) => {
                    warnings.push((key.path.to_string(), reason));
                }
                Err(ResurrectSkip::Fatal(err)) => return Err(err),
            }
        }
        Ok(warnings)
    }

    fn try_resurrect_applied(
        &mut self,
        peer: DeviceId,
        key: &EntryKey,
    ) -> Result<(), ResurrectSkip> {
        let Some(current) = self.db.repo().entry(key).map_err(EngineError::from_db)? else {
            return Err(ResurrectSkip::Precondition);
        };
        if !current.is_deleted() || current.modified_by != peer {
            return Err(ResurrectSkip::Precondition);
        }
        let Some(config) = self
            .db
            .repo()
            .mount_config(key.mount)
            .map_err(EngineError::from_db)?
        else {
            return Err(ResurrectSkip::Precondition);
        };
        let Some(root) = config.local_path.as_ref() else {
            return Err(ResurrectSkip::Precondition);
        };
        relay_fs::MountMarker::verify(root, config.mount.id).map_err(EngineError::from)?;

        let dest = match resolve_os_path(root, &key.path).map_err(EngineError::from)? {
            Some(existing) => existing,
            None => to_os_path(root, &key.path).map_err(EngineError::from)?,
        };
        match fs::symlink_metadata(&dest) {
            Ok(_) => return Err(ResurrectSkip::Precondition),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(ResurrectSkip::Warn(format!(
                    "could not restore applied delete: {err}"
                )));
            }
        }

        let history = self.db.repo().history(key).map_err(EngineError::from_db)?;
        let Some(prior) = history
            .iter()
            .rev()
            .find(|h| h.sequence != current.sequence)
        else {
            return Err(ResurrectSkip::Precondition);
        };
        let (object, size, executable) = match &prior.content {
            EntryContent::File {
                object,
                size,
                executable,
            } => (*object, *size, *executable),
            _ => return Err(ResurrectSkip::Precondition),
        };
        if !self.store.contains(&object) {
            return Err(ResurrectSkip::Warn(format!(
                "could not restore applied delete: object {object} is missing"
            )));
        }

        self.restore_file_version(key, root, object, size, executable, Some(&current))
            .map_err(|err| {
                ResurrectSkip::Warn(format!("could not restore applied delete: {err}"))
            })?;
        Ok(())
    }

    pub(crate) fn persist_delete_hold(
        &mut self,
        peer: DeviceId,
        space: relay_core::SpaceId,
        mount: relay_core::MountId,
        deletions: usize,
        live: usize,
        paths: (&[relay_core::LogicalPath], &[relay_core::LogicalPath]),
    ) -> Result<(), EngineError> {
        let now = self.clock.now_ms();
        let (held, applied) = paths;
        self.db
            .transaction(|repo| {
                repo.upsert_delete_hold(peer, space, mount, deletions, live, now)?;
                repo.replace_delete_hold_paths(peer, space, mount, held)?;
                repo.insert_delete_hold_applied_paths(peer, space, mount, applied)?;
                Ok(())
            })
            .map_err(EngineError::from_db)
    }

    pub(crate) fn clear_delete_hold_paths(
        &mut self,
        peer: DeviceId,
        space: relay_core::SpaceId,
        mount: relay_core::MountId,
    ) -> Result<(), EngineError> {
        self.db
            .transaction(|repo| repo.replace_delete_hold_paths(peer, space, mount, &[]))
            .map_err(EngineError::from_db)
    }

    pub(crate) fn clear_delete_holds_for_peer_space(
        &mut self,
        peer: DeviceId,
        space: relay_core::SpaceId,
    ) -> Result<(), EngineError> {
        self.db
            .transaction(|repo| repo.clear_delete_holds_for_peer_space(peer, space))
            .map_err(EngineError::from_db)
    }

    pub fn status(&self) -> Result<Status, EngineError> {
        let local = self
            .db
            .repo()
            .local_device()
            .map_err(EngineError::from_db)?
            .ok_or(EngineError::NotInitialized)?;
        let last_sequence = Sequence(local.next_sequence.0.saturating_sub(1));
        let listed = self.mounts(None)?;
        let mut mounts = Vec::with_capacity(listed.len());
        for (space, config) in listed {
            let entries = self.db.repo().entries_for_mount(config.mount.id)?;
            let live_entries = entries.iter().filter(|e| !e.is_deleted()).count();
            let tombstones = entries.len() - live_entries;
            let (marker_ok, marker_state) = match &config.local_path {
                None => (false, "NO_PATH".to_owned()),
                Some(path) => match MountMarker::verify(path, config.mount.id) {
                    Ok(_) => (true, "OK".to_owned()),
                    Err(relay_fs::FsError::MarkerMissing(_)) => (false, "MISSING".to_owned()),
                    Err(relay_fs::FsError::MarkerMismatch { .. }) => (false, "MISMATCH".to_owned()),
                    Err(relay_fs::FsError::MarkerInvalid { .. }) => (false, "INVALID".to_owned()),
                    Err(relay_fs::FsError::MountRootMissing(_)) => {
                        (false, "ROOT_MISSING".to_owned())
                    }
                    Err(relay_fs::FsError::NotADirectory(_)) => {
                        (false, "NOT_A_DIRECTORY".to_owned())
                    }
                    Err(_) => (false, "ERROR".to_owned()),
                },
            };
            let state = self.db.repo().mount_state(config.mount.id)?;
            mounts.push(MountStatus {
                space: space.name,
                mount: config.mount.name,
                path: config.local_path,
                marker_ok,
                marker_state,
                live_entries,
                tombstones,
                last_scan_ms: state.as_ref().and_then(|s| s.last_scan_ms),
                last_error: state.and_then(|s| s.last_error),
            });
        }
        let object_count = self.store.list()?.len() as u64;
        let mut peers = Vec::new();
        for peer in self.db.repo().list_peers()? {
            let mut spaces = Vec::new();
            for space_id in self.db.repo().shared_space_ids(peer.device.id)? {
                let Some(space) = self.db.repo().space(space_id)? else {
                    continue;
                };
                let progress = self.db.repo().sync_progress(peer.device.id, space_id)?;
                let our_latest_seq = self.db.repo().max_sequence_in_space(space_id)?;
                spaces.push(crate::reports::PeerSpaceStatus {
                    space: space.name,
                    received_seq: progress.received_seq,
                    acked_seq: progress.acked_seq,
                    our_latest_seq,
                    last_sync_ms: progress.last_sync_ms,
                });
            }
            spaces.sort_by(|a, b| a.space.cmp(&b.space));
            peers.push(crate::reports::PeerStatus {
                name: peer.device.name,
                id: peer.device.id,
                addresses: peer.addresses,
                spaces,
            });
        }
        Ok(Status {
            device: self.device.clone(),
            mounts,
            object_count,
            last_sequence,
            peers,
        })
    }

    pub(crate) fn lookup_mount(
        &self,
        space: &str,
        mount: &str,
    ) -> Result<(Space, MountConfig), EngineError> {
        let space_rec = self
            .db
            .repo()
            .space_by_name(space)?
            .ok_or_else(|| EngineError::UnknownSpace(space.to_owned()))?;
        let mount_rec = self
            .db
            .repo()
            .mount_by_name(space_rec.id, mount)?
            .ok_or_else(|| EngineError::UnknownMount {
                space: space.to_owned(),
                mount: mount.to_owned(),
            })?;
        let config = self
            .db
            .repo()
            .mount_config(mount_rec.id)?
            .ok_or(EngineError::MountNotLocal)?;
        Ok((space_rec, config))
    }

    fn ensure_writable(&self) -> Result<(), EngineError> {
        if self.lock.is_none() {
            Err(EngineError::ReadOnly)
        } else {
            Ok(())
        }
    }

    fn can_adopt_leftover_marker(&self, root: &Path) -> Result<bool, EngineError> {
        let marker = match MountMarker::read(root) {
            Ok(marker) => marker,
            Err(_) => return Ok(false),
        };
        if marker.created_by != self.device.id {
            return Ok(false);
        }
        // Ours, for a mount that no longer exists or is no longer attached
        // here (a `remove_mount` that could not delete the marker).
        Ok(self
            .db
            .repo()
            .mount_config(marker.mount)?
            .is_none_or(|config| config.local_path.is_none()))
    }
}

fn ensure_layout(home: &Path) -> Result<(), EngineError> {
    fs::create_dir_all(home)?;
    fs::create_dir_all(home.join(LOGS_DIR))?;
    Ok(())
}

fn load_or_generate_identity(dir: &Path) -> Result<DeviceIdentity, EngineError> {
    if DeviceIdentity::exists(dir) {
        return Ok(DeviceIdentity::load(dir)?);
    }
    match DeviceIdentity::generate(dir) {
        Ok(identity) => Ok(identity),
        Err(relay_crypto::CryptoError::KeyExists(_)) => Ok(DeviceIdentity::load(dir)?),
        Err(err) => Err(err.into()),
    }
}

fn require_matching_identity(
    home: &Path,
    expected: DeviceId,
) -> Result<DeviceIdentity, EngineError> {
    let dir = home.join(IDENTITY_DIR);
    let identity = match DeviceIdentity::load(&dir) {
        Ok(identity) => identity,
        Err(relay_crypto::CryptoError::Io { .. }) => {
            return Err(EngineError::StaleIdentity {
                home: home.to_path_buf(),
            });
        }
        Err(err) => return Err(err.into()),
    };
    if identity.device_id() != expected {
        return Err(EngineError::StaleIdentity {
            home: home.to_path_buf(),
        });
    }
    Ok(identity)
}

/// Read-only connections cannot migrate. An older schema (e.g. the first
/// open after an app update) is upgraded through a normal writer open, which
/// serializes migrations on the home lock, then the read-only open is retried.
fn open_read_only_db(home: &Path, db_path: &Path) -> Result<Database, EngineError> {
    let deadline = std::time::Instant::now() + SCHEMA_UPGRADE_WAIT;
    let mut upgraded = false;
    loop {
        match Database::open_read_only(db_path) {
            Err(relay_db::DbError::SchemaTooOld { .. }) if !upgraded => {
                match Engine::open_for_config(home) {
                    Ok(_) => upgraded = true,
                    Err(EngineError::Busy { .. }) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(err) => return Err(err),
                }
            }
            other => return Ok(other?),
        }
    }
}

fn acquire_lock(home: &Path) -> Result<File, EngineError> {
    try_lock_file(home, LOCK_FILE)
}

/// Exclusive lock held for the duration of [`Engine::run`] / [`Engine::watch`].
///
/// The writer lock (`relay.lock`) is released while the loop runs so another
/// process can `Engine::open` and commit config. This lock keeps two run
/// loops from overlapping.
pub(crate) fn acquire_run_lock(home: &Path) -> Result<File, EngineError> {
    try_lock_file(home, RUN_LOCK_FILE)
}

/// How long a lock that looks taken is retried before reporting `Busy`.
/// Another thread's fork briefly holds a copy of a just-released lock fd
/// until the child execs, which is enough to fail a single attempt.
const LOCK_RETRY: Duration = Duration::from_millis(100);

/// Take the exclusive lock on `home/name` without waiting on a real holder.
fn try_lock_file(home: &Path, name: &str) -> Result<File, EngineError> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join(name))?;
    let deadline = Instant::now() + LOCK_RETRY;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(EngineError::Busy {
                    home: home.to_path_buf(),
                });
            }
            Err(fs::TryLockError::Error(err)) => return Err(EngineError::Io(err)),
        }
    }
}

fn run_lock_held(home: &Path) -> Result<bool, EngineError> {
    match acquire_run_lock(home) {
        Ok(file) => {
            drop(file);
            Ok(false)
        }
        Err(EngineError::Busy { .. }) => Ok(true),
        Err(err) => Err(err),
    }
}

impl Engine {
    pub(crate) fn release_writer_lock(&self) -> Result<(), EngineError> {
        if let Some(file) = &self.lock {
            file.unlock()?;
        }
        Ok(())
    }

    pub(crate) fn try_reacquire_writer_lock(&self) -> Result<(), EngineError> {
        let Some(file) = &self.lock else {
            return Ok(());
        };
        match file.try_lock() {
            Ok(()) => Ok(()),
            Err(fs::TryLockError::WouldBlock) => Ok(()),
            Err(fs::TryLockError::Error(err)) => Err(EngineError::Io(err)),
        }
    }
}

enum ResurrectSkip {
    Precondition,
    Warn(String),
    Fatal(EngineError),
}

impl From<EngineError> for ResurrectSkip {
    fn from(err: EngineError) -> Self {
        Self::Fatal(err)
    }
}

fn restore_expected_stat(
    store: &ObjectStore,
    dest: &Path,
    current: Option<&EntryRecord>,
) -> Result<Option<relay_core::StatHint>, EngineError> {
    let Some(record) = current else {
        return Ok(None);
    };
    if record.is_deleted() || !matches!(record.content, EntryContent::File { .. }) {
        return Ok(None);
    }
    match record.stat {
        Some(stat) => Ok(Some(stat)),
        None if let Some(stat) = crate::materialize::dehydrated_stat(dest) => {
            if relay_fs::cloud::placeholder_object(dest) == record.content.object() {
                Ok(Some(stat))
            } else {
                Err(EngineError::DestinationChanged(dest.to_path_buf()))
            }
        }
        None => match store.hash_file(dest, None) {
            Ok(outcome) if Some(outcome.id) == record.content.object() => Ok(Some(outcome.stat)),
            Ok(_) => Err(EngineError::DestinationChanged(dest.to_path_buf())),
            Err(StoreError::SourceChanged { path }) => Err(EngineError::DestinationChanged(path)),
            Err(StoreError::Io { path, source }) if source.kind() == io::ErrorKind::NotFound => {
                Err(EngineError::DestinationChanged(path))
            }
            Err(err) => Err(err.into()),
        },
    }
}

fn live_entry_matches_disk(
    store: &ObjectStore,
    root: &Path,
    record: &EntryRecord,
) -> Result<bool, EngineError> {
    let dest = match resolve_os_path(root, &record.key.path)? {
        Some(path) => path,
        None => return Ok(false),
    };
    match &record.content {
        EntryContent::Directory => match fs::symlink_metadata(&dest) {
            Ok(meta) => Ok(meta.is_dir() && !meta.file_type().is_symlink()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(EngineError::Io(err)),
        },
        EntryContent::File { object, .. } => match record.stat {
            Some(stat) => match fs::symlink_metadata(&dest) {
                Ok(meta) => Ok(relay_core::StatHint::from_metadata(&meta) == stat),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(err) => Err(EngineError::Io(err)),
            },
            // A placeholder without data: the bytes are not on disk.
            None if crate::materialize::dehydrated_stat(&dest).is_some() => Ok(false),
            None => match store.hash_file(&dest, None) {
                Ok(outcome) => Ok(outcome.id == *object),
                Err(StoreError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                    Ok(false)
                }
                Err(StoreError::SourceChanged { .. }) => Ok(false),
                Err(err) => Err(err.into()),
            },
        },
        EntryContent::Symlink { .. } => match fs::symlink_metadata(&dest) {
            Ok(meta) => Ok(meta.file_type().is_symlink()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(EngineError::Io(err)),
        },
        EntryContent::Deleted => Ok(false),
    }
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

fn content_kind_name(content: &EntryContent) -> &'static str {
    match content {
        EntryContent::File { .. } => "file",
        EntryContent::Directory => "directory",
        EntryContent::Symlink { .. } => "symlink",
        EntryContent::Deleted => "deleted",
    }
}
