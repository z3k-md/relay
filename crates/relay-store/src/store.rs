use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use relay_core::{ObjectId, StatHint};
use tempfile::{NamedTempFile, TempPath};

use crate::error::StoreError;

const OBJECTS_DIR: &str = "objects";
const TMP_DIR: &str = "tmp";
const CHUNK_SIZE: usize = 64 * 1024;

/// Content-addressed immutable object store on a local filesystem.
#[derive(Debug, Clone)]
pub struct ObjectStore {
    root: PathBuf,
}

/// Result of hashing a live user file into the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutOutcome {
    pub id: ObjectId,
    pub size: u64,
    pub stat: StatHint,
    pub already_present: bool,
}

/// Outcome of a mark-and-sweep pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub removed: usize,
    pub bytes_freed: u64,
    pub kept: usize,
}

impl ObjectStore {
    /// Open (or create) a store at `root`. Creates `objects/` and `tmp/` if missing.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        let store = Self { root };
        create_dir(&store.objects_dir())?;
        create_dir(&store.tmp_dir())?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_for(&self, id: &ObjectId) -> PathBuf {
        let hex = id.to_hex();
        self.objects_dir()
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(hex)
    }

    pub fn contains(&self, id: &ObjectId) -> bool {
        self.path_for(id).is_file()
    }

    pub fn put_bytes(&self, data: &[u8]) -> Result<ObjectId, StoreError> {
        let id = ObjectId::of(data);
        if self.contains(&id) {
            return Ok(id);
        }

        let tmp_dir = self.ensure_tmp()?;
        let mut tmp = create_tmp(&tmp_dir)?;
        tmp.write_all(data).map_err(|e| io_err(tmp.path(), e))?;
        durable_install(self, id, tmp)?;
        Ok(id)
    }

    /// Hash + copy a live user file into the store.
    ///
    /// The source is re-stated after the copy. Any change to size, mtime, or
    /// file identity (or a swap of the path for a symlink) is `SourceChanged`,
    /// so we never index a half-written editor save.
    pub fn put_file(
        &self,
        source: &Path,
        expected: Option<&StatHint>,
    ) -> Result<PutOutcome, StoreError> {
        let (mut file, before) = open_live_file(source, expected)?;

        let tmp_dir = self.ensure_tmp()?;
        let mut tmp = create_tmp(&tmp_dir)?;
        let tmp_path = tmp.path().to_owned();
        let (id, size) = copy_hashed(&mut file, &mut tmp, source, &tmp_path)?;

        if !stat_unchanged(&before, &file, source)? {
            // `tmp` drops and removes the in-flight file.
            return Err(StoreError::SourceChanged {
                path: source.to_owned(),
            });
        }

        let already_present = durable_install(self, id, tmp)?;
        Ok(PutOutcome {
            id,
            size,
            stat: before,
            already_present,
        })
    }

    /// Same stable-read checks as [`put_file`], but nothing is written to the store.
    ///
    /// `already_present` reports whether the store already has the hashed object.
    pub fn hash_file(
        &self,
        source: &Path,
        expected: Option<&StatHint>,
    ) -> Result<PutOutcome, StoreError> {
        let (mut file, before) = open_live_file(source, expected)?;
        let (id, size) = copy_hashed(&mut file, &mut io::sink(), source, source)?;

        if !stat_unchanged(&before, &file, source)? {
            return Err(StoreError::SourceChanged {
                path: source.to_owned(),
            });
        }

        Ok(PutOutcome {
            id,
            size,
            stat: before,
            already_present: self.contains(&id),
        })
    }

    pub fn open_object(&self, id: &ObjectId) -> Result<File, StoreError> {
        let path = self.path_for(id);
        File::open(&path).map_err(|e| map_not_found(*id, path, e))
    }

    pub fn read(&self, id: &ObjectId) -> Result<Vec<u8>, StoreError> {
        let path = self.path_for(id);
        let data = fs::read(&path).map_err(|e| map_not_found(*id, path, e))?;
        let actual = ObjectId::of(&data);
        if actual != *id {
            return Err(StoreError::Corrupt { id: *id, actual });
        }
        Ok(data)
    }

    pub fn verify(&self, id: &ObjectId) -> Result<(), StoreError> {
        let path = self.path_for(id);
        let actual = hash_path(&path).map_err(|e| map_not_found(*id, path, e))?;
        if actual != *id {
            return Err(StoreError::Corrupt { id: *id, actual });
        }
        Ok(())
    }

    pub fn size_of(&self, id: &ObjectId) -> Result<u64, StoreError> {
        let path = self.path_for(id);
        fs::metadata(&path)
            .map(|meta| meta.len())
            .map_err(|e| map_not_found(*id, path, e))
    }

    /// How many objects are stored, counted without collecting their ids.
    pub fn count(&self) -> Result<u64, StoreError> {
        let objects = self.objects_dir();
        if !objects.exists() {
            return Ok(0);
        }
        let mut count = 0;
        for entry in walkdir::WalkDir::new(&objects) {
            let entry = entry.map_err(walkdir_err)?;
            if entry.file_type().is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.parse::<ObjectId>().is_ok())
            {
                count += 1;
            }
        }
        Ok(count)
    }

    /// All stored object ids. Stray non-hex names under `objects/` are ignored.
    pub fn list(&self) -> Result<Vec<ObjectId>, StoreError> {
        let objects = self.objects_dir();
        if !objects.exists() {
            return Ok(Vec::new());
        }

        let mut ids = Vec::new();
        for entry in walkdir::WalkDir::new(&objects) {
            let entry = entry.map_err(walkdir_err)?;
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str() else {
                continue;
            };
            if let Ok(id) = name.parse::<ObjectId>() {
                ids.push(id);
            }
        }
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    /// Remove objects not in `live`, unless the file's mtime is within `grace`.
    ///
    /// Young objects are kept because a concurrent `put` may not have committed
    /// its id to the index yet.
    pub fn sweep(
        &self,
        live: &HashSet<ObjectId>,
        grace: Duration,
    ) -> Result<SweepReport, StoreError> {
        let now = SystemTime::now();
        let mut removed = 0;
        let mut bytes_freed = 0;
        let mut kept = 0;

        for id in self.list()? {
            let path = self.path_for(&id);
            if live.contains(&id) {
                kept += 1;
                continue;
            }

            let meta = match fs::metadata(&path) {
                Ok(meta) => meta,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_err(&path, e)),
            };

            if is_within_grace(&meta, now, grace) {
                kept += 1;
                continue;
            }

            let size = meta.len();
            fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
            removed += 1;
            bytes_freed += size;
        }

        Ok(SweepReport {
            removed,
            bytes_freed,
            kept,
        })
    }

    /// Directory used for in-flight writes (`<root>/tmp`).
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join(TMP_DIR)
    }

    /// Reserve a unique empty file under [`Self::tmp_dir`] for an in-flight write.
    ///
    /// The file is created immediately so the name cannot collide. Leftovers are
    /// reaped by [`Self::clean_tmp`].
    pub fn tmp_path(&self) -> Result<PathBuf, StoreError> {
        let dir = self.ensure_tmp()?;
        let tmp = tempfile::Builder::new()
            .prefix("recv-")
            .tempfile_in(&dir)
            .map_err(|e| io_err(&dir, e))?;
        let tmp_path = tmp.into_temp_path();
        match tmp_path.keep() {
            Ok(path) => Ok(path),
            Err(e) => Err(io_err(&dir, e.error)),
        }
    }

    /// Rehash `tmp`, require it equals `expected`, fsync, and atomically publish
    /// it to the object path. `tmp` is removed on success, hash mismatch, or
    /// when the object is already present.
    pub fn import_verified(&self, tmp: &Path, expected: &ObjectId) -> Result<(), StoreError> {
        let actual = match hash_path(tmp) {
            Ok(id) => id,
            Err(e) => return Err(io_err(tmp, e)),
        };
        if actual != *expected {
            let _ = fs::remove_file(tmp);
            return Err(StoreError::Corrupt {
                id: *expected,
                actual,
            });
        }

        if self.contains(expected) {
            let _ = fs::remove_file(tmp);
            return Ok(());
        }

        // Windows' FlushFileBuffers needs a writable handle; a read-only one
        // fails with "Access denied".
        let file = fs::OpenOptions::new()
            .write(true)
            .open(tmp)
            .map_err(|e| io_err(tmp, e))?;
        file.sync_all().map_err(|e| io_err(tmp, e))?;
        drop(file);

        let dest = self.path_for(expected);
        if dest.is_file() {
            let _ = fs::remove_file(tmp);
            return Ok(());
        }
        if let Some(parent) = dest.parent() {
            create_dir(parent)?;
        }
        match fs::rename(tmp, &dest) {
            Ok(()) => {
                fsync_dir_best_effort(dest.parent());
                Ok(())
            }
            Err(e) => {
                let _ = fs::remove_file(tmp);
                if dest.is_file() {
                    self.verify(expected)?;
                    Ok(())
                } else {
                    Err(io_err(&dest, e))
                }
            }
        }
    }

    /// Delete leftover tmp files whose mtime is older than `older_than`.
    pub fn clean_tmp(&self, older_than: Duration) -> Result<usize, StoreError> {
        let dir = self.tmp_dir();
        if !dir.exists() {
            return Ok(0);
        }

        let now = SystemTime::now();
        let mut removed = 0;
        let entries = fs::read_dir(&dir).map_err(|e| io_err(&dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&dir, e))?;
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_err(&path, e)),
            };
            if !meta.is_file() {
                continue;
            }
            if !is_older_than(&meta, now, older_than) {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(&path, e)),
            }
        }
        Ok(removed)
    }

    fn objects_dir(&self) -> PathBuf {
        self.root.join(OBJECTS_DIR)
    }

    fn ensure_tmp(&self) -> Result<PathBuf, StoreError> {
        let dir = self.tmp_dir();
        create_dir(&dir)?;
        Ok(dir)
    }
}

pub(crate) fn open_live_file(
    source: &Path,
    expected: Option<&StatHint>,
) -> Result<(File, StatHint), StoreError> {
    let file = File::open(source).map_err(|e| io_err(source, e))?;
    let before = StatHint::from_metadata(&file.metadata().map_err(|e| io_err(source, e))?);
    if expected.is_some_and(|hint| before != *hint) {
        return Err(StoreError::SourceChanged {
            path: source.to_owned(),
        });
    }
    Ok((file, before))
}

/// Both the open handle and the path must still match `before`.
///
/// `symlink_metadata` is used so a TOCTOU swap of the path for a symlink
/// cannot pass (handle metadata follows; path metadata would then differ).
/// Metadata errors after a successful open are treated as "changed".
pub(crate) fn stat_unchanged(
    before: &StatHint,
    handle: &File,
    path: &Path,
) -> Result<bool, StoreError> {
    let Ok(handle_meta) = handle.metadata() else {
        return Ok(false);
    };
    let Ok(path_meta) = fs::symlink_metadata(path) else {
        return Ok(false);
    };
    Ok(StatHint::from_metadata(&handle_meta) == *before
        && StatHint::from_metadata(&path_meta) == *before)
}

pub(crate) fn copy_hashed(
    src: &mut impl Read,
    dst: &mut impl Write,
    src_path: &Path,
    dst_path: &Path,
) -> Result<(ObjectId, u64), StoreError> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; CHUNK_SIZE];
    let mut size = 0u64;
    loop {
        let n = src.read(&mut buf).map_err(|e| io_err(src_path, e))?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n]).map_err(|e| io_err(dst_path, e))?;
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((ObjectId::from(hasher.finalize()), size))
}

fn hash_path(path: &Path) -> io::Result<ObjectId> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; CHUNK_SIZE];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(ObjectId::from(hasher.finalize()))
}

pub(crate) fn create_tmp(dir: &Path) -> Result<NamedTempFile, StoreError> {
    tempfile::Builder::new()
        .prefix("put-")
        .tempfile_in(dir)
        .map_err(|e| io_err(dir, e))
}

/// `sync_all` the temp file, then atomically publish it (or drop it if the
/// object already exists). Returns `true` when the object was already present.
/// The temp is always removed on error or when skipped as a duplicate.
fn durable_install(
    store: &ObjectStore,
    id: ObjectId,
    tmp: NamedTempFile,
) -> Result<bool, StoreError> {
    tmp.as_file()
        .sync_all()
        .map_err(|e| io_err(tmp.path(), e))?;

    let dest = store.path_for(&id);
    if dest.is_file() {
        return Ok(true);
    }

    if let Some(parent) = dest.parent() {
        create_dir(parent)?;
    }

    // Close the handle before rename: Windows cannot move an open file.
    let tmp_path: TempPath = tmp.into_temp_path();
    match fs::rename(&tmp_path, &dest) {
        Ok(()) => {
            // TempPath would try to unlink the old name; that path is gone.
            let _ = tmp_path.keep();
            fsync_dir_best_effort(dest.parent());
            Ok(false)
        }
        Err(e) => {
            // Windows: rename onto an existing file fails. Another put of the
            // same bytes may have won the race; that is success if it verifies.
            if dest.is_file() {
                store.verify(&id)?;
                Ok(true)
            } else {
                Err(io_err(&dest, e))
            }
        }
    }
}

fn fsync_dir_best_effort(dir: Option<&Path>) {
    let Some(dir) = dir else {
        return;
    };
    fsync_dir(dir);
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) {
    if let Ok(file) = File::open(dir) {
        let _ = file.sync_all();
    }
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) {}

fn is_within_grace(meta: &fs::Metadata, now: SystemTime, grace: Duration) -> bool {
    if grace.is_zero() {
        return false;
    }
    match meta.modified() {
        Ok(mtime) => match now.duration_since(mtime) {
            Ok(age) => age < grace,
            Err(_) => true,
        },
        Err(_) => false,
    }
}

fn is_older_than(meta: &fs::Metadata, now: SystemTime, older_than: Duration) -> bool {
    match meta.modified() {
        Ok(mtime) => match now.duration_since(mtime) {
            Ok(age) => age > older_than,
            Err(_) => false,
        },
        Err(_) => false,
    }
}

pub(crate) fn create_dir(path: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(path).map_err(|e| io_err(path, e))
}

pub(crate) fn io_err(path: impl AsRef<Path>, source: io::Error) -> StoreError {
    StoreError::Io {
        path: path.as_ref().to_owned(),
        source,
    }
}

fn map_not_found(id: ObjectId, path: PathBuf, source: io::Error) -> StoreError {
    if source.kind() == io::ErrorKind::NotFound {
        StoreError::NotFound(id)
    } else {
        StoreError::Io { path, source }
    }
}

fn walkdir_err(err: walkdir::Error) -> StoreError {
    let path = err.path().unwrap_or_else(|| Path::new("")).to_owned();
    match err.into_io_error() {
        Some(source) => StoreError::Io { path, source },
        None => StoreError::Io {
            path,
            source: io::Error::other("walkdir error"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    fn setup() -> (tempfile::TempDir, ObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(dir.path()).unwrap();
        (dir, store)
    }

    fn tmp_file_count(store: &ObjectStore) -> usize {
        fs::read_dir(store.tmp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .count()
    }

    fn object_file_count(store: &ObjectStore) -> usize {
        walkdir::WalkDir::new(store.objects_dir())
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.parse::<ObjectId>().is_ok())
            })
            .count()
    }

    fn assert_corrupt(err: StoreError, expected_id: ObjectId, actual_bytes: &[u8]) {
        match err {
            StoreError::Corrupt { id, actual } => {
                assert_eq!(id, expected_id);
                assert_eq!(actual, ObjectId::of(actual_bytes));
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn open_creates_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let store = ObjectStore::open(&root).unwrap();
        assert_eq!(store.root(), root.as_path());
        assert!(root.join(OBJECTS_DIR).is_dir());
        assert!(root.join(TMP_DIR).is_dir());
    }

    #[test]
    fn put_bytes_round_trip_and_hash() {
        let (_dir, store) = setup();
        let data = b"hello relay";
        let id = store.put_bytes(data).unwrap();
        assert_eq!(id, ObjectId::of(data));
        assert_eq!(id, ObjectId::from(blake3::hash(data)));
        assert!(store.contains(&id));
        assert_eq!(store.read(&id).unwrap(), data);
        assert_eq!(store.size_of(&id).unwrap(), data.len() as u64);
        store.verify(&id).unwrap();

        let hex = id.to_hex();
        let expected = store
            .root()
            .join(OBJECTS_DIR)
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(&hex);
        assert_eq!(store.path_for(&id), expected);
        assert!(expected.is_file());

        let mut file = store.open_object(&id).unwrap();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, data);
    }

    #[test]
    fn empty_file_round_trip() {
        let (_dir, store) = setup();
        let id = store.put_bytes(b"").unwrap();
        assert_eq!(id, ObjectId::of(b""));
        assert_eq!(store.read(&id).unwrap(), b"");
        assert_eq!(store.size_of(&id).unwrap(), 0);

        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("empty");
        fs::write(&src, b"").unwrap();
        let out = store.put_file(&src, None).unwrap();
        assert_eq!(out.id, id);
        assert!(out.already_present);
        assert_eq!(out.size, 0);
    }

    #[test]
    fn put_file_round_trip() {
        let (_dir, store) = setup();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("note.txt");
        fs::write(&src, b"from disk").unwrap();

        let meta = fs::metadata(&src).unwrap();
        let expected = StatHint::from_metadata(&meta);
        let out = store.put_file(&src, Some(&expected)).unwrap();
        assert_eq!(out.id, ObjectId::of(b"from disk"));
        assert_eq!(out.size, 9);
        assert_eq!(out.stat, expected);
        assert!(!out.already_present);
        assert_eq!(store.read(&out.id).unwrap(), b"from disk");
        assert_eq!(tmp_file_count(&store), 0);
    }

    #[test]
    fn put_deduplicates_to_one_file() {
        let (_dir, store) = setup();
        let id1 = store.put_bytes(b"same").unwrap();
        let id2 = store.put_bytes(b"same").unwrap();
        assert_eq!(id1, id2);
        assert_eq!(object_file_count(&store), 1);

        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("same.bin");
        fs::write(&src, b"same").unwrap();
        let first = store.put_file(&src, None).unwrap();
        assert!(first.already_present);
        let second = store.put_file(&src, None).unwrap();
        assert!(second.already_present);
        assert_eq!(first.id, id1);
        assert_eq!(object_file_count(&store), 1);
        assert_eq!(store.list().unwrap(), vec![id1]);
        assert_eq!(tmp_file_count(&store), 0);
    }

    #[test]
    fn hash_file_wrong_expected_is_source_changed_and_does_not_store() {
        let (_dir, store) = setup();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("live.txt");
        fs::write(&src, b"payload").unwrap();

        let wrong = StatHint {
            size: 1,
            mtime_ns: 0,
            file_id: None,
            ctime_ns: None,
        };
        let err = store.hash_file(&src, Some(&wrong)).unwrap_err();
        assert!(
            matches!(err, StoreError::SourceChanged { ref path } if path == &src),
            "{err:?}"
        );
        assert_eq!(tmp_file_count(&store), 0);
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn hash_file_id_matches_put_file_and_does_not_store() {
        let (_dir, store) = setup();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("note.txt");
        fs::write(&src, b"from disk").unwrap();

        let hashed = store.hash_file(&src, None).unwrap();
        assert_eq!(hashed.id, ObjectId::of(b"from disk"));
        assert_eq!(hashed.size, 9);
        assert!(!hashed.already_present);
        assert!(store.list().unwrap().is_empty());
        assert_eq!(tmp_file_count(&store), 0);

        let put = store.put_file(&src, Some(&hashed.stat)).unwrap();
        assert_eq!(put.id, hashed.id);
        assert_eq!(put.size, hashed.size);
        assert_eq!(put.stat, hashed.stat);
        assert!(!put.already_present);

        let hashed_again = store.hash_file(&src, Some(&put.stat)).unwrap();
        assert_eq!(hashed_again.id, put.id);
        assert!(hashed_again.already_present);
    }

    #[test]
    fn put_file_wrong_expected_is_source_changed_and_leaves_no_tmp() {
        let (_dir, store) = setup();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("live.txt");
        fs::write(&src, b"payload").unwrap();

        let wrong = StatHint {
            size: 1,
            mtime_ns: 0,
            file_id: None,
            ctime_ns: None,
        };
        let err = store.put_file(&src, Some(&wrong)).unwrap_err();
        assert!(
            matches!(err, StoreError::SourceChanged { ref path } if path == &src),
            "{err:?}"
        );
        assert_eq!(tmp_file_count(&store), 0);
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn stat_unchanged_detects_in_place_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("editing.txt");
        fs::write(&path, b"v1").unwrap();

        let file = File::open(&path).unwrap();
        let before = StatHint::from_metadata(&file.metadata().unwrap());
        assert!(stat_unchanged(&before, &file, &path).unwrap());

        // Size change is enough; no sleep, no mtime race.
        fs::write(&path, b"v1-edited").unwrap();
        assert!(!stat_unchanged(&before, &file, &path).unwrap());
    }

    #[test]
    fn read_and_verify_detect_corruption() {
        let (_dir, store) = setup();
        let id = store.put_bytes(b"hello").unwrap();
        fs::write(store.path_for(&id), b"hallo").unwrap();

        assert_corrupt(store.read(&id).unwrap_err(), id, b"hallo");
        assert_corrupt(store.verify(&id).unwrap_err(), id, b"hallo");
    }

    #[test]
    fn missing_object_is_not_found() {
        let (_dir, store) = setup();
        let id = ObjectId::of(b"absent");
        assert!(!store.contains(&id));
        assert!(matches!(store.read(&id), Err(StoreError::NotFound(got)) if got == id));
        assert!(matches!(store.verify(&id), Err(StoreError::NotFound(got)) if got == id));
        assert!(matches!(store.size_of(&id), Err(StoreError::NotFound(got)) if got == id));
        assert!(matches!(
            store.open_object(&id),
            Err(StoreError::NotFound(got)) if got == id
        ));
    }

    #[test]
    fn list_returns_all_ids_and_ignores_junk() {
        let (_dir, store) = setup();
        let a = store.put_bytes(b"alpha").unwrap();
        let b = store.put_bytes(b"bravo").unwrap();

        fs::write(store.objects_dir().join("readme.txt"), b"nope").unwrap();
        fs::write(store.objects_dir().join("zzzz"), b"short").unwrap();
        let shard = store.objects_dir().join("ab").join("cd");
        fs::create_dir_all(&shard).unwrap();
        fs::write(shard.join("not-hex-at-all"), b"junk").unwrap();
        fs::write(shard.join("deadbeef"), b"too-short").unwrap();

        let mut listed = store.list().unwrap();
        listed.sort();
        let mut expect = vec![a, b];
        expect.sort();
        assert_eq!(listed, expect);
    }

    #[test]
    fn sweep_removes_unreferenced_keeps_live_and_honors_grace() {
        let (_dir, store) = setup();
        let live_id = store.put_bytes(b"keep-me").unwrap();
        let stale = store.put_bytes(b"sweep-me").unwrap();
        let stale_size = store.size_of(&stale).unwrap();

        let mut live = HashSet::new();
        live.insert(live_id);

        let report = store.sweep(&live, Duration::ZERO).unwrap();
        assert_eq!(
            report,
            SweepReport {
                removed: 1,
                bytes_freed: stale_size,
                kept: 1,
            }
        );
        assert!(store.contains(&live_id));
        assert!(!store.contains(&stale));

        let young = store.put_bytes(b"brand-new").unwrap();
        let report = store.sweep(&live, Duration::from_secs(60 * 60)).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.kept, 2);
        assert!(store.contains(&young));

        let report = store.sweep(&live, Duration::ZERO).unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(report.kept, 1);
        assert!(!store.contains(&young));
        assert!(store.contains(&live_id));
    }

    #[test]
    fn clean_tmp_removes_old_files_only() {
        let (_dir, store) = setup();
        let old_path = store.tmp_dir().join("old.tmp");
        let young_path = store.tmp_dir().join("young.tmp");
        fs::write(&old_path, b"old").unwrap();
        fs::write(&young_path, b"young").unwrap();

        let file = OpenOptions::new().write(true).open(&old_path).unwrap();
        let past = SystemTime::now() - Duration::from_secs(3600);
        file.set_times(fs::FileTimes::new().set_modified(past))
            .unwrap();
        drop(file);

        let removed = store.clean_tmp(Duration::from_secs(60)).unwrap();
        assert_eq!(removed, 1);
        assert!(!old_path.exists());
        assert!(young_path.exists());

        let file = OpenOptions::new().write(true).open(&young_path).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(past))
            .unwrap();
        drop(file);

        let removed = store.clean_tmp(Duration::from_secs(60)).unwrap();
        assert_eq!(removed, 1);
        assert!(!young_path.exists());
        assert_eq!(tmp_file_count(&store), 0);
    }

    #[test]
    fn tmp_path_is_unique_under_tmp() {
        let (_dir, store) = setup();
        let a = store.tmp_path().unwrap();
        let b = store.tmp_path().unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with(store.tmp_dir()));
        assert!(
            a.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("recv-"))
        );
        assert!(a.is_file());
        assert!(b.is_file());
        assert_eq!(tmp_file_count(&store), 2);
    }

    #[test]
    fn import_verified_publishes_matching_bytes() {
        let (_dir, store) = setup();
        let data = b"imported-object";
        let id = ObjectId::of(data);
        let tmp = store.tmp_path().unwrap();
        fs::write(&tmp, data).unwrap();
        store.import_verified(&tmp, &id).unwrap();
        assert!(!tmp.exists());
        assert_eq!(store.read(&id).unwrap(), data);
        assert_eq!(tmp_file_count(&store), 0);
    }

    #[test]
    fn import_verified_rejects_hash_mismatch() {
        let (_dir, store) = setup();
        let expected = ObjectId::of(b"wanted");
        let tmp = store.tmp_path().unwrap();
        fs::write(&tmp, b"other").unwrap();
        let err = store.import_verified(&tmp, &expected).unwrap_err();
        assert_corrupt(err, expected, b"other");
        assert!(!store.contains(&expected));
        assert!(!tmp.exists());
    }

    #[test]
    fn import_verified_ok_when_already_present() {
        let (_dir, store) = setup();
        let id = store.put_bytes(b"dup").unwrap();
        let tmp = store.tmp_path().unwrap();
        fs::write(&tmp, b"dup").unwrap();
        store.import_verified(&tmp, &id).unwrap();
        assert_eq!(object_file_count(&store), 1);
        assert!(!tmp.exists());
        assert_eq!(store.read(&id).unwrap(), b"dup");
    }

    #[test]
    fn abandoned_recv_tmp_is_reaped_by_clean_tmp() {
        let (_dir, store) = setup();
        let tmp = store.tmp_path().unwrap();
        fs::write(&tmp, b"leftover").unwrap();

        let file = OpenOptions::new().write(true).open(&tmp).unwrap();
        let past = SystemTime::now() - Duration::from_secs(3600);
        file.set_times(fs::FileTimes::new().set_modified(past))
            .unwrap();
        drop(file);

        let removed = store.clean_tmp(Duration::from_secs(60)).unwrap();
        assert_eq!(removed, 1);
        assert!(!tmp.exists());
        assert_eq!(tmp_file_count(&store), 0);
    }
}
