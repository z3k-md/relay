//! Integration layer: local index, object store, and filesystem scans.

mod clock;
mod error;
mod reports;
mod scan;
mod watch;

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use relay_core::{DeviceId, EntryKey, MOUNT_MARKER, validate_name};
use relay_db::Database;
use relay_fs::{MaterializeOptions, MountMarker, materialize_file, resolve_os_path, to_os_path};
use relay_policy::MountRules;
use relay_store::StoreError;

pub use clock::{Clock, ManualClock, SystemClock};
pub use error::EngineError;
pub use relay_core::{
    Device, EntryContent, EntryKind, EntryRecord, LogicalPath, Mount, ObjectId, Sequence, Space,
    VectorOrdering,
};
pub use relay_db::{HistoryRecord, MountConfig};
pub use relay_fs::{FsError, ScanWarning};
pub use relay_store::ObjectStore;
pub use relay_crypto::DeviceIdentity;
pub use reports::{
    GcReport, MASS_DELETE_DENOMINATOR, MASS_DELETE_MIN_COUNT, MASS_DELETE_NUMERATOR, MountStatus,
    ScanOptions, ScanReport, Status, VerifyReport, Warning,
};
pub use watch::{WatchEvent, WatchOptions};

const DB_FILE: &str = "relay.db";
const STORE_DIR: &str = "store";
const LOGS_DIR: &str = "logs";
const IDENTITY_DIR: &str = "identity";
const LOCK_FILE: &str = "relay.lock";
const TMP_CLEAN_AGE: Duration = Duration::from_secs(60 * 60);

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
    /// Exclusive lock on `<home>/relay.lock`. `None` for a read-only engine.
    lock: Option<File>,
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
        let store = ObjectStore::open(home.join(STORE_DIR))?;
        Ok(Engine {
            home: home.to_path_buf(),
            db,
            store,
            device,
            clock,
            config: EngineConfig::default(),
            lock: Some(lock),
        })
    }

    pub fn open(home: &Path) -> Result<Engine, EngineError> {
        let db_path = home.join(DB_FILE);
        if !db_path.is_file() {
            return Err(EngineError::NotInitialized);
        }
        ensure_layout(home)?;
        let lock = acquire_lock(home)?;
        let db = Database::open(&db_path)?;
        let local = db
            .repo()
            .local_device()
            .map_err(EngineError::from_db)?
            .ok_or(EngineError::NotInitialized)?;
        let identity = require_matching_identity(home, local.device.id)?;
        let _ = identity;
        let store = ObjectStore::open(home.join(STORE_DIR))?;
        Ok(Engine {
            home: home.to_path_buf(),
            db,
            store,
            device: local.device,
            clock: Arc::new(SystemClock),
            config: EngineConfig::default(),
            lock: Some(lock),
        })
    }

    /// Open the home for reads only. Does not take the writer lock.
    pub fn open_read_only(home: &Path) -> Result<Engine, EngineError> {
        let db_path = home.join(DB_FILE);
        if !db_path.is_file() {
            return Err(EngineError::NotInitialized);
        }
        let db = Database::open_read_only(&db_path)?;
        let local = db
            .repo()
            .local_device()
            .map_err(EngineError::from_db)?
            .ok_or(EngineError::NotInitialized)?;
        let identity = require_matching_identity(home, local.device.id)?;
        let _ = identity;
        let store = ObjectStore::open(home.join(STORE_DIR))?;
        Ok(Engine {
            home: home.to_path_buf(),
            db,
            store,
            device: local.device,
            clock: Arc::new(SystemClock),
            config: EngineConfig::default(),
            lock: None,
        })
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Engine {
        self.clock = clock;
        self
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

    pub fn scan(
        &mut self,
        space: &str,
        mount: &str,
        opts: ScanOptions,
    ) -> Result<ScanReport, EngineError> {
        self.scan_mount(space, mount, opts)
    }

    /// Incremental scan of `paths` (and the scopes `scan_paths` derives).
    pub fn scan_paths(
        &mut self,
        space: &str,
        mount: &str,
        paths: &[LogicalPath],
        opts: ScanOptions,
    ) -> Result<ScanReport, EngineError> {
        self.scan_paths_inner(space, mount, paths, opts)
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

        let dest = match resolve_os_path(&local_path, path)? {
            Some(existing) => existing,
            None => to_os_path(&local_path, path)?,
        };
        let current = self.db.repo().entry(&key)?;
        let expected_existing = restore_expected_stat(&self.store, &dest, current.as_ref())?;

        let mut reader = self.store.open_object(&object)?;
        let stat = match materialize_file(
            &mut reader,
            &dest,
            object,
            executable,
            expected_existing.as_ref(),
            MaterializeOptions {
                mount_root: &local_path,
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
                    current.as_ref(),
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

    pub fn verify_objects(&self) -> Result<VerifyReport, EngineError> {
        let live = self.db.repo().live_objects()?;
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
        Ok(Status {
            device: self.device.clone(),
            mounts,
            object_count,
            last_sequence,
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
        Ok(self.db.repo().mount_config(marker.mount)?.is_none())
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

fn require_matching_identity(home: &Path, expected: DeviceId) -> Result<DeviceIdentity, EngineError> {
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

fn acquire_lock(home: &Path) -> Result<File, EngineError> {
    let path = home.join(LOCK_FILE);
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(EngineError::Busy {
            home: home.to_path_buf(),
        }),
        Err(fs::TryLockError::Error(err)) => Err(EngineError::Io(err)),
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
