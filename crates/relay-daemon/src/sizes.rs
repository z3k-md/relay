//! Folder sizes for remote browsing (D42).
//!
//! A listing never waits on these. The managing device asks for
//! [`relay_core::remote::RemoteCall::FolderSizes`] after it lists a folder,
//! gets whatever is counted so far, and asks again until every folder is
//! done. Counting runs here, on the device that owns the files, on a few
//! background threads. Finished counts are cached; a folder nobody has asked
//! about for a while stops being counted.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// Folders counted at once.
const WORKERS: usize = 2;
/// A finished count is served as is for this long, then counted again.
const FRESH: Duration = Duration::from_secs(10 * 60);
/// A count nobody has asked about for this long is dropped.
const ABANDONED: Duration = Duration::from_secs(15);
/// Entries walked between checks for abandonment.
const CHECK_EVERY: u32 = 4096;
/// Finished counts kept; the oldest go first.
const CACHE_MAX: usize = 4096;

/// How far one folder's count has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Progress {
    pub bytes: u64,
    pub files: u64,
    pub done: bool,
}

#[derive(Default)]
pub(crate) struct Sizer {
    shared: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    counted: HashMap<PathBuf, Counted>,
    jobs: HashMap<PathBuf, Arc<Job>>,
    queue: VecDeque<Arc<Job>>,
    workers: usize,
}

#[derive(Clone, Copy)]
struct Counted {
    bytes: u64,
    files: u64,
    at: Instant,
}

struct Job {
    path: PathBuf,
    bytes: AtomicU64,
    files: AtomicU64,
    asked: Mutex<Instant>,
}

impl Job {
    fn abandoned(&self) -> bool {
        lock(&self.asked).elapsed() >= ABANDONED
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Sizer {
    /// Progress on each of `folders`, starting counts that are missing or
    /// stale. A stale count is reported, not done, until it is redone.
    pub(crate) fn sizes(&self, folders: &[PathBuf]) -> Vec<Progress> {
        let now = Instant::now();
        let mut state = lock(&self.shared);
        let mut started = Vec::new();
        let progress = folders
            .iter()
            .map(|path| {
                let counted = state.counted.get(path).copied();
                if let Some(c) = counted
                    && now.duration_since(c.at) < FRESH
                {
                    return Progress {
                        bytes: c.bytes,
                        files: c.files,
                        done: true,
                    };
                }
                let job = match state.jobs.get(path) {
                    Some(job) => {
                        *lock(&job.asked) = now;
                        Arc::clone(job)
                    }
                    None => {
                        let job = Arc::new(Job {
                            path: path.clone(),
                            bytes: AtomicU64::new(0),
                            files: AtomicU64::new(0),
                            asked: Mutex::new(now),
                        });
                        state.jobs.insert(path.clone(), Arc::clone(&job));
                        started.push(Arc::clone(&job));
                        job
                    }
                };
                match counted {
                    Some(c) => Progress {
                        bytes: c.bytes,
                        files: c.files,
                        done: false,
                    },
                    None => Progress {
                        bytes: job.bytes.load(Ordering::Relaxed),
                        files: job.files.load(Ordering::Relaxed),
                        done: false,
                    },
                }
            })
            .collect();
        // The folder just opened goes ahead of older requests, in order.
        for job in started.into_iter().rev() {
            state.queue.push_front(job);
        }
        while state.workers < WORKERS.min(state.queue.len()) {
            let shared = Arc::clone(&self.shared);
            let spawned = thread::Builder::new()
                .name("relay-sizes".into())
                .spawn(move || work(&shared));
            if spawned.is_err() {
                break;
            }
            state.workers += 1;
        }
        progress
    }
}

fn work(shared: &Mutex<State>) {
    loop {
        let job = {
            let mut state = lock(shared);
            match state.queue.pop_front() {
                Some(job) => job,
                None => {
                    state.workers -= 1;
                    return;
                }
            }
        };
        let finished = !job.abandoned() && count(&job);
        let mut state = lock(shared);
        state.jobs.remove(&job.path);
        if finished {
            state.counted.insert(
                job.path.clone(),
                Counted {
                    bytes: job.bytes.load(Ordering::Relaxed),
                    files: job.files.load(Ordering::Relaxed),
                    at: Instant::now(),
                },
            );
            if state.counted.len() > CACHE_MAX
                && let Some(oldest) = state
                    .counted
                    .iter()
                    .min_by_key(|(_, c)| c.at)
                    .map(|(path, _)| path.clone())
            {
                state.counted.remove(&oldest);
            }
        }
    }
}

/// Walk `job.path`, adding up allocated space and files as it goes. Does not
/// follow symlinks or junctions, stays on one filesystem, and counts a hard
/// linked file once. False if the job was abandoned partway.
fn count(job: &Job) -> bool {
    let Ok(root) = fs::symlink_metadata(&job.path) else {
        return true;
    };
    let device = device_of(&root);
    job.bytes
        .fetch_add(allocated(&job.path, &root), Ordering::Relaxed);
    let mut linked = HashSet::new();
    let mut dirs = vec![job.path.clone()];
    let mut walked = 0u32;
    while let Some(dir) = dirs.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            walked += 1;
            if walked >= CHECK_EVERY {
                walked = 0;
                if job.abandoned() {
                    return false;
                }
            }
            // Does not follow a symlink.
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let file_type = meta.file_type();
            if file_type.is_dir() && device_of(&meta) != device {
                continue;
            }
            if !file_type.is_dir()
                && let Some(id) = hard_link_id(&meta)
                && !linked.insert(id)
            {
                continue;
            }
            let path = entry.path();
            job.bytes
                .fetch_add(allocated(&path, &meta), Ordering::Relaxed);
            if file_type.is_dir() {
                dirs.push(path);
            } else if file_type.is_file() {
                job.files.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    true
}

/// Space `path` takes on disk: its allocated blocks, so a sparse,
/// compressed, or cloud-only file counts for less than its length.
#[cfg(unix)]
pub(crate) fn allocated(_path: &Path, meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks().saturating_mul(512)
}

/// Windows has no allocation in `Metadata`. Cloud placeholders take nothing
/// until recalled; compressed and sparse files ask the filesystem; anything
/// else is its length rounded up to NTFS's default 4 KiB cluster.
#[cfg(windows)]
pub(crate) fn allocated(path: &Path, meta: &fs::Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
    const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x4_0000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    const CLUSTER: u64 = 4096;
    let attributes = meta.file_attributes();
    if meta.is_dir()
        || attributes
            & (FILE_ATTRIBUTE_OFFLINE
                | FILE_ATTRIBUTE_RECALL_ON_OPEN
                | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
            != 0
    {
        return 0;
    }
    if attributes & (FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_COMPRESSED) != 0
        && let Ok(size) = filesize::file_real_size(path)
    {
        return size;
    }
    meta.len().div_ceil(CLUSTER).saturating_mul(CLUSTER)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn allocated(_path: &Path, meta: &fs::Metadata) -> u64 {
    meta.len()
}

#[cfg(unix)]
fn device_of(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}

/// Windows: junctions and mounted folders are reparse points, which the walk
/// already does not follow.
#[cfg(not(unix))]
fn device_of(_meta: &fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn hard_link_id(meta: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (meta.nlink() > 1).then(|| (meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn hard_link_id(_meta: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_done(sizer: &Sizer, folders: &[PathBuf]) -> Vec<Progress> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let progress = sizer.sizes(folders);
            if progress.iter().all(|p| p.done) {
                return progress;
            }
            assert!(Instant::now() < deadline, "sizes never finished");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn counts_folders_in_the_background_and_caches() {
        let root = tempfile::TempDir::new().unwrap();
        let big = root.path().join("big");
        let small = root.path().join("small");
        let empty = root.path().join("empty");
        fs::create_dir_all(big.join("deep/er")).unwrap();
        fs::create_dir(&small).unwrap();
        fs::create_dir(&empty).unwrap();
        fs::write(big.join("a.bin"), vec![1u8; 100_000]).unwrap();
        fs::write(big.join("deep/er/b.bin"), vec![1u8; 50_000]).unwrap();
        fs::write(small.join("c.txt"), b"hi").unwrap();
        let folders = vec![big.clone(), small.clone(), empty.clone()];

        let sizer = Sizer::default();
        let first = sizer.sizes(&folders);
        assert_eq!(first.len(), 3);
        let done = wait_done(&sizer, &folders);
        assert_eq!(done[0].files, 2);
        assert_eq!(done[1].files, 1);
        assert_eq!(done[2].files, 0);
        assert!(done[0].bytes > done[1].bytes);
        if cfg!(unix) {
            // Allocated, so at least the bytes written.
            assert!(done[0].bytes >= 150_000, "{}", done[0].bytes);
        }

        // Served from the cache: a new file is not seen until it goes stale.
        fs::write(small.join("d.txt"), b"more").unwrap();
        assert_eq!(sizer.sizes(&folders), done);
    }

    #[cfg(unix)]
    #[test]
    fn counts_hard_links_once_and_skips_symlinks() {
        let root = tempfile::TempDir::new().unwrap();
        let dir = root.path().join("d");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("a.bin"), vec![1u8; 64_000]).unwrap();
        fs::hard_link(dir.join("a.bin"), dir.join("b.bin")).unwrap();
        let elsewhere = root.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::write(elsewhere.join("huge.bin"), vec![1u8; 1_000_000]).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("link")).unwrap();

        let sizer = Sizer::default();
        let done = wait_done(&sizer, std::slice::from_ref(&dir));
        assert_eq!(done[0].files, 1, "hard link counted once");
        assert!(
            done[0].bytes < 500_000,
            "followed a symlink: {}",
            done[0].bytes
        );
    }

    #[test]
    fn a_missing_folder_counts_as_empty() {
        let root = tempfile::TempDir::new().unwrap();
        let gone = root.path().join("gone");
        let done = wait_done(&Sizer::default(), &[gone]);
        assert_eq!(
            done[0],
            Progress {
                bytes: 0,
                files: 0,
                done: true
            }
        );
    }
}
