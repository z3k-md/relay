use std::path::Path;

use relay_core::StatHint;

use crate::error::StoreError;
use crate::store::{ObjectStore, PutOutcome};

/// Default auto-commit: 1024 staged objects or 256 MiB, whichever first.
const DEFAULT_MAX_OBJECTS: usize = 1024;
const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Batched [`ObjectStore::put_file`].
///
/// On Apple, objects are staged with a plain `fsync` and only published after
/// a volume-wide `F_FULLFSYNC` barrier in [`Self::commit`]. Elsewhere each
/// `put_file` is the same as [`ObjectStore::put_file`] (fully durable) and
/// `commit` is a no-op.
///
/// Dropping a batch without [`Self::commit`] installs nothing (Apple) or
/// leaves already-durable puts in place (other platforms).
pub struct PutBatch {
    store: ObjectStore,
    #[cfg(target_vendor = "apple")]
    apple: apple::State,
}

impl ObjectStore {
    /// Start a batched put. See [`PutBatch`].
    pub fn batch(&self) -> PutBatch {
        PutBatch::with_cap(self.clone(), DEFAULT_MAX_OBJECTS, DEFAULT_MAX_BYTES)
    }

    #[cfg(all(test, target_vendor = "apple"))]
    pub(crate) fn batch_with_cap(&self, max_objects: usize, max_bytes: u64) -> PutBatch {
        PutBatch::with_cap(self.clone(), max_objects, max_bytes)
    }
}

impl PutBatch {
    fn with_cap(store: ObjectStore, max_objects: usize, max_bytes: u64) -> Self {
        #[cfg(not(target_vendor = "apple"))]
        let _ = (max_objects, max_bytes);
        Self {
            store,
            #[cfg(target_vendor = "apple")]
            apple: apple::State::new(max_objects, max_bytes),
        }
    }

    /// Same stable-read checks and [`crate::StoreError::SourceChanged`]
    /// semantics as [`ObjectStore::put_file`].
    pub fn put_file(
        &mut self,
        source: &Path,
        expected: Option<&StatHint>,
    ) -> Result<PutOutcome, StoreError> {
        #[cfg(target_vendor = "apple")]
        {
            apple::put_file(self, source, expected)
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            self.store.put_file(source, expected)
        }
    }

    /// Publish every staged object. No-op when there is nothing staged, and
    /// on non-Apple platforms.
    pub fn commit(&mut self) -> Result<(), StoreError> {
        #[cfg(target_vendor = "apple")]
        {
            apple::commit(self)
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            let _ = self;
            Ok(())
        }
    }
}

#[cfg(target_vendor = "apple")]
mod apple {
    use std::collections::HashSet;
    use std::fs::{self, File};
    use std::path::Path;

    use relay_core::{ObjectId, StatHint};
    use tempfile::TempPath;

    use super::PutBatch;
    use crate::error::StoreError;
    use crate::store::{
        ObjectStore, PutOutcome, copy_hashed, create_dir, create_tmp, io_err, open_live_file,
        stat_unchanged,
    };

    pub(super) struct State {
        staged: Vec<Staged>,
        ids: HashSet<ObjectId>,
        bytes: u64,
        max_objects: usize,
        max_bytes: u64,
    }

    struct Staged {
        id: ObjectId,
        tmp: TempPath,
    }

    impl State {
        pub(super) fn new(max_objects: usize, max_bytes: u64) -> Self {
            Self {
                staged: Vec::new(),
                ids: HashSet::new(),
                bytes: 0,
                max_objects,
                max_bytes,
            }
        }

        fn at_cap(&self) -> bool {
            self.staged.len() >= self.max_objects || self.bytes >= self.max_bytes
        }
    }

    pub(super) fn put_file(
        batch: &mut PutBatch,
        source: &Path,
        expected: Option<&StatHint>,
    ) -> Result<PutOutcome, StoreError> {
        let (mut file, before) = open_live_file(source, expected)?;

        let tmp_dir = batch.store.tmp_dir();
        create_dir(&tmp_dir)?;
        let mut tmp = create_tmp(&tmp_dir)?;
        let tmp_path = tmp.path().to_owned();
        let (id, size) = copy_hashed(&mut file, &mut tmp, source, &tmp_path)?;

        if !stat_unchanged(&before, &file, source)? {
            return Err(StoreError::SourceChanged {
                path: source.to_owned(),
            });
        }

        if batch.store.contains(&id) || batch.apple.ids.contains(&id) {
            return Ok(PutOutcome {
                id,
                size,
                stat: before,
                already_present: true,
            });
        }

        // Same fault point as `durable_install`, so an injected store
        // failure surfaces at the same put on every platform.
        let dest = batch.store.path_for(&id);
        relay_core::faults::check(relay_core::faults::FaultPoint::StorePut, &dest)
            .map_err(|e| io_err(&dest, e))?;

        rustix::fs::fsync(tmp.as_file()).map_err(|e| io_err(tmp.path(), e.into()))?;
        let tmp = tmp.into_temp_path();

        batch.apple.ids.insert(id);
        batch.apple.bytes = batch.apple.bytes.saturating_add(size);
        batch.apple.staged.push(Staged { id, tmp });

        if batch.apple.at_cap() {
            commit(batch)?;
        }

        Ok(PutOutcome {
            id,
            size,
            stat: before,
            already_present: false,
        })
    }

    pub(super) fn commit(batch: &mut PutBatch) -> Result<(), StoreError> {
        if batch.apple.staged.is_empty() {
            return Ok(());
        }

        // Data in every staged tmp has been plain-fsync'd. This drive-cache
        // flush must land *before* any rename: `contains` treats a dest path
        // as durable, so the name must not appear until the bytes are.
        full_fsync_file(&batch.apple.staged[0].tmp)?;

        let staged = std::mem::take(&mut batch.apple.staged);
        batch.apple.ids.clear();
        batch.apple.bytes = 0;

        let mut dirs = HashSet::new();
        for item in staged {
            let dest = batch.store.path_for(&item.id);
            if let Some(parent) = dest.parent() {
                dirs.insert(parent.to_owned());
            }
            publish_tmp(&batch.store, item.id, item.tmp)?;
        }

        for dir in dirs {
            if let Ok(file) = File::open(&dir) {
                let _ = rustix::fs::fsync(&file);
            }
        }

        if let Ok(root) = File::open(batch.store.root()) {
            let _ = rustix::fs::fcntl_fullfsync(&root);
        }
        Ok(())
    }

    fn publish_tmp(
        store: &ObjectStore,
        id: ObjectId,
        tmp_path: TempPath,
    ) -> Result<(), StoreError> {
        let dest = store.path_for(&id);
        if dest.is_file() {
            return Ok(());
        }
        if let Some(parent) = dest.parent() {
            create_dir(parent)?;
        }
        match fs::rename(&tmp_path, &dest) {
            Ok(()) => {
                let _ = tmp_path.keep();
                Ok(())
            }
            Err(e) => {
                if dest.is_file() {
                    store.verify(&id)?;
                    Ok(())
                } else {
                    Err(io_err(&dest, e))
                }
            }
        }
    }

    fn full_fsync_file(path: &Path) -> Result<(), StoreError> {
        let file = File::options()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| io_err(path, e))?;
        rustix::fs::fcntl_fullfsync(&file).map_err(|e| io_err(path, e.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use relay_core::{ObjectId, StatHint};

    use crate::error::StoreError;

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
        walkdir::WalkDir::new(store.root().join("objects"))
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

    fn write_src(dir: &std::path::Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn batch_commit_installs_and_verifies() {
        let (_dir, store) = setup();
        let src_dir = tempfile::tempdir().unwrap();
        let a = write_src(src_dir.path(), "a.txt", b"alpha");
        let b = write_src(src_dir.path(), "b.txt", b"bravo");

        let mut batch = store.batch();
        let oa = batch.put_file(&a, None).unwrap();
        let ob = batch.put_file(&b, None).unwrap();
        assert!(!oa.already_present);
        assert!(!ob.already_present);
        batch.commit().unwrap();

        assert_eq!(store.read(&oa.id).unwrap(), b"alpha");
        assert_eq!(store.read(&ob.id).unwrap(), b"bravo");
        store.verify(&oa.id).unwrap();
        store.verify(&ob.id).unwrap();
        assert_eq!(tmp_file_count(&store), 0);
    }

    #[test]
    fn batch_duplicates_collapse() {
        let (_dir, store) = setup();
        let src_dir = tempfile::tempdir().unwrap();
        let a = write_src(src_dir.path(), "a.bin", b"same");
        let b = write_src(src_dir.path(), "b.bin", b"same");

        let mut batch = store.batch();
        let first = batch.put_file(&a, None).unwrap();
        let second = batch.put_file(&b, None).unwrap();
        assert_eq!(first.id, second.id);
        assert!(!first.already_present);
        assert!(second.already_present);
        batch.commit().unwrap();

        assert_eq!(object_file_count(&store), 1);
        assert_eq!(tmp_file_count(&store), 0);
        store.verify(&first.id).unwrap();
    }

    #[test]
    fn batch_source_changed_leaves_no_tmp() {
        let (_dir, store) = setup();
        let src_dir = tempfile::tempdir().unwrap();
        let good = write_src(src_dir.path(), "good.txt", b"keep");
        let bad = write_src(src_dir.path(), "bad.txt", b"payload");

        let mut batch = store.batch();
        let kept = batch.put_file(&good, None).unwrap();
        let wrong = StatHint {
            size: 1,
            mtime_ns: 0,
            file_id: None,
            ctime_ns: None,
        };
        let err = batch.put_file(&bad, Some(&wrong)).unwrap_err();
        assert!(
            matches!(err, StoreError::SourceChanged { ref path } if path == &bad),
            "{err:?}"
        );

        #[cfg(target_vendor = "apple")]
        {
            assert!(!store.contains(&kept.id));
            assert_eq!(tmp_file_count(&store), 1);
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            assert!(store.contains(&kept.id));
            assert_eq!(tmp_file_count(&store), 0);
        }

        drop(batch);
        assert_eq!(tmp_file_count(&store), 0);
        #[cfg(target_vendor = "apple")]
        {
            assert!(!store.contains(&kept.id));
            assert_eq!(object_file_count(&store), 0);
        }
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn drop_without_commit_installs_nothing() {
        let (_dir, store) = setup();
        let src_dir = tempfile::tempdir().unwrap();
        let src = write_src(src_dir.path(), "a.txt", b"x");
        let id;
        {
            let mut batch = store.batch();
            id = batch.put_file(&src, None).unwrap().id;
            assert!(!store.contains(&id));
            assert_eq!(tmp_file_count(&store), 1);
        }
        assert!(!store.contains(&id));
        assert_eq!(tmp_file_count(&store), 0);
        assert_eq!(object_file_count(&store), 0);
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn dest_exists_before_commit_is_success() {
        let (_dir, store) = setup();
        let src_dir = tempfile::tempdir().unwrap();
        let src = write_src(src_dir.path(), "a.txt", b"payload");

        let mut batch = store.batch();
        let out = batch.put_file(&src, None).unwrap();
        assert!(!out.already_present);
        assert!(!store.contains(&out.id));

        store.put_file(&src, None).unwrap();
        assert!(store.contains(&out.id));

        batch.commit().unwrap();
        store.verify(&out.id).unwrap();
        assert_eq!(tmp_file_count(&store), 0);
        assert_eq!(object_file_count(&store), 1);
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn auto_commit_at_object_cap() {
        let (_dir, store) = setup();
        let src_dir = tempfile::tempdir().unwrap();
        let a = write_src(src_dir.path(), "a", b"one");
        let b = write_src(src_dir.path(), "b", b"two");
        let c = write_src(src_dir.path(), "c", b"three");

        let mut batch = store.batch_with_cap(2, u64::MAX);
        let oa = batch.put_file(&a, None).unwrap();
        assert!(!store.contains(&oa.id));
        let ob = batch.put_file(&b, None).unwrap();
        assert!(store.contains(&oa.id));
        assert!(store.contains(&ob.id));
        let oc = batch.put_file(&c, None).unwrap();
        assert!(!store.contains(&oc.id));

        batch.commit().unwrap();
        assert!(store.contains(&oc.id));
        store.verify(&oa.id).unwrap();
        store.verify(&ob.id).unwrap();
        store.verify(&oc.id).unwrap();
        assert_eq!(tmp_file_count(&store), 0);
    }
}
