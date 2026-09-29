use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::error::FsError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchSignal {
    /// Paths under `root` may have changed. Never authoritative.
    Changed { root: PathBuf, paths: Vec<PathBuf> },
    /// Events may have been lost for `root` (overflow, backend error, root
    /// itself moved/removed): the caller must do a full scan.
    Rescan { root: PathBuf, reason: String },
}

pub struct MountWatcher {
    watcher: RecommendedWatcher,
    roots: Arc<Mutex<Vec<PathBuf>>>,
}

impl MountWatcher {
    pub fn new(sink: Sender<WatchSignal>) -> Result<Self, FsError> {
        let roots = Arc::new(Mutex::new(Vec::new()));
        let callback_roots = Arc::clone(&roots);
        let watcher = RecommendedWatcher::new(
            move |result| handle_notify_event(&sink, &callback_roots, result),
            Config::default(),
        )
        .map_err(|err| watch_error(Path::new("."), err))?;
        Ok(Self { watcher, roots })
    }

    /// Recursively watch `root` (caller passes a canonical path).
    pub fn watch(&mut self, root: &Path) -> Result<(), FsError> {
        {
            let mut roots = lock_roots(&self.roots);
            if !roots.iter().any(|existing| existing == root) {
                roots.push(root.to_path_buf());
            }
        }
        if let Err(err) = self.watcher.watch(root, RecursiveMode::Recursive) {
            let mut roots = lock_roots(&self.roots);
            roots.retain(|existing| existing != root);
            return Err(watch_error(root, err));
        }
        Ok(())
    }

    pub fn unwatch(&mut self, root: &Path) -> Result<(), FsError> {
        self.watcher
            .unwatch(root)
            .map_err(|err| watch_error(root, err))?;
        lock_roots(&self.roots).retain(|existing| existing != root);
        Ok(())
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        lock_roots(&self.roots).clone()
    }
}

fn handle_notify_event(
    sink: &Sender<WatchSignal>,
    roots: &Mutex<Vec<PathBuf>>,
    result: Result<Event, notify::Error>,
) {
    let roots = lock_roots(roots);
    let ignore_case = platform_ignore_case();
    match result {
        Ok(event) => dispatch_event(sink, &roots, ignore_case, event),
        Err(err) => dispatch_error(sink, &roots, ignore_case, err),
    }
}

fn dispatch_event(sink: &Sender<WatchSignal>, roots: &[PathBuf], ignore_case: bool, event: Event) {
    if event.need_rescan() {
        let affected = affected_roots(roots, &event.paths, ignore_case);
        for root in affected {
            send(
                sink,
                WatchSignal::Rescan {
                    root,
                    reason: "backend requested rescan".to_owned(),
                },
            );
        }
        return;
    }
    if is_ignored_access(event.kind) {
        return;
    }

    let mut changes: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    let mut rescans: Vec<PathBuf> = Vec::new();
    for path in event.paths {
        let Some(root) = owning_root(roots, &path, ignore_case) else {
            continue;
        };
        let root = root.to_path_buf();
        if paths_equal(&root, &path, ignore_case) {
            if !rescans.iter().any(|existing| existing == &root) {
                rescans.push(root);
            }
            continue;
        }
        push_grouped(&mut changes, root, path);
    }

    for root in rescans {
        changes.retain(|(changed_root, _)| changed_root != &root);
        send(
            sink,
            WatchSignal::Rescan {
                root,
                reason: "root moved or removed".to_owned(),
            },
        );
    }
    for (root, paths) in changes {
        send(sink, WatchSignal::Changed { root, paths });
    }
}

fn dispatch_error(
    sink: &Sender<WatchSignal>,
    roots: &[PathBuf],
    ignore_case: bool,
    err: notify::Error,
) {
    let reason = err.to_string();
    for root in affected_roots(roots, &err.paths, ignore_case) {
        send(
            sink,
            WatchSignal::Rescan {
                root,
                reason: reason.clone(),
            },
        );
    }
}

fn affected_roots(roots: &[PathBuf], paths: &[PathBuf], ignore_case: bool) -> Vec<PathBuf> {
    if paths.is_empty() {
        return roots.to_vec();
    }
    let mut affected = Vec::new();
    for path in paths {
        if let Some(root) = owning_root(roots, path, ignore_case) {
            let root = root.to_path_buf();
            if !affected.iter().any(|existing| existing == &root) {
                affected.push(root);
            }
        }
    }
    if affected.is_empty() {
        roots.to_vec()
    } else {
        affected
    }
}

fn is_ignored_access(kind: EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => false,
        EventKind::Access(_) => true,
        _ => false,
    }
}

fn push_grouped(groups: &mut Vec<(PathBuf, Vec<PathBuf>)>, root: PathBuf, path: PathBuf) {
    if let Some((_, paths)) = groups.iter_mut().find(|(existing, _)| existing == &root) {
        paths.push(path);
        return;
    }
    groups.push((root, vec![path]));
}

fn send(sink: &Sender<WatchSignal>, signal: WatchSignal) {
    let _ = sink.send(signal);
}

fn lock_roots(roots: &Mutex<Vec<PathBuf>>) -> std::sync::MutexGuard<'_, Vec<PathBuf>> {
    roots.lock().unwrap_or_else(|err| err.into_inner())
}

fn watch_error(path: &Path, err: notify::Error) -> FsError {
    FsError::io(path, io::Error::other(err.to_string()))
}

fn platform_ignore_case() -> bool {
    cfg!(any(windows, target_os = "macos"))
}

/// Owning watched root of `path`: the longest component-wise ancestor.
///
/// On macOS and Windows, components compare case-insensitively. `ignore_case`
/// is passed in so the rule can be unit-tested on Linux.
pub(crate) fn owning_root<'a>(
    roots: &'a [PathBuf],
    path: &Path,
    ignore_case: bool,
) -> Option<&'a Path> {
    roots
        .iter()
        .filter(|root| is_component_ancestor(root, path, ignore_case))
        .max_by_key(|root| root.components().count())
        .map(PathBuf::as_path)
}

fn is_component_ancestor(root: &Path, path: &Path, ignore_case: bool) -> bool {
    let root_comps: Vec<Component<'_>> = root.components().collect();
    let path_comps: Vec<Component<'_>> = path.components().collect();
    if path_comps.len() < root_comps.len() {
        return false;
    }
    root_comps
        .iter()
        .zip(path_comps.iter())
        .all(|(a, b)| components_equal(*a, *b, ignore_case))
}

fn paths_equal(a: &Path, b: &Path, ignore_case: bool) -> bool {
    let a: Vec<Component<'_>> = a.components().collect();
    let b: Vec<Component<'_>> = b.components().collect();
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(l, r)| components_equal(*l, *r, ignore_case))
}

fn components_equal(a: Component<'_>, b: Component<'_>, ignore_case: bool) -> bool {
    if a == b {
        return true;
    }
    if !ignore_case {
        return false;
    }
    match (a.as_os_str().to_str(), b.as_os_str().to_str()) {
        (Some(left), Some(right)) => left.to_lowercase() == right.to_lowercase(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn owning_root_prefers_longest_match() {
        let roots = vec![
            PathBuf::from("/data"),
            PathBuf::from("/data/mount"),
            PathBuf::from("/other"),
        ];
        assert_eq!(
            owning_root(&roots, Path::new("/data/mount/a.txt"), false),
            Some(Path::new("/data/mount"))
        );
        assert_eq!(
            owning_root(&roots, Path::new("/data/file"), false),
            Some(Path::new("/data"))
        );
        assert_eq!(owning_root(&roots, Path::new("/outside/x"), false), None);
    }

    #[test]
    fn owning_root_case_insensitive_mapping() {
        let roots = vec![PathBuf::from("/Users/Ada/Project")];
        let nested = Path::new("/users/ada/project/src/Main.rs");
        assert_eq!(
            owning_root(&roots, nested, true),
            Some(Path::new("/Users/Ada/Project"))
        );
        assert_eq!(owning_root(&roots, nested, false), None);
        assert!(paths_equal(
            Path::new("/Users/Ada/Project"),
            Path::new("/users/ada/project"),
            true
        ));
        assert!(!paths_equal(
            Path::new("/Users/Ada/Project"),
            Path::new("/users/ada/project"),
            false
        ));
    }

    #[test]
    fn owning_root_respects_component_boundaries() {
        let roots = vec![PathBuf::from("/data/mount")];
        assert_eq!(
            owning_root(&roots, Path::new("/data/mountain/x"), false),
            None
        );
    }

    #[test]
    fn watch_and_unwatch_update_roots() {
        use std::sync::mpsc;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let (tx, _rx) = mpsc::channel();
        let mut watcher = MountWatcher::new(tx).unwrap();
        watcher.watch(dir.path()).unwrap();
        assert_eq!(watcher.roots(), vec![dir.path().to_path_buf()]);
        watcher.unwatch(dir.path()).unwrap();
        assert!(watcher.roots().is_empty());
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::*;
        use std::fs;
        use std::sync::mpsc::{self, Receiver};
        use std::time::{Duration, Instant};
        use tempfile::tempdir;

        const TIMEOUT: Duration = Duration::from_secs(5);

        fn drain_until(
            rx: &Receiver<WatchSignal>,
            timeout: Duration,
            mut pred: impl FnMut(&[WatchSignal]) -> bool,
        ) -> Vec<WatchSignal> {
            let deadline = Instant::now() + timeout;
            let mut collected = Vec::new();
            loop {
                if pred(&collected) {
                    return collected;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return collected;
                }
                match rx.recv_timeout(remaining) {
                    Ok(signal) => collected.push(signal),
                    Err(mpsc::RecvTimeoutError::Timeout) => return collected,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return collected,
                }
            }
        }

        fn same_path(left: &Path, right: &Path) -> bool {
            if left == right {
                return true;
            }
            if let (Ok(a), Ok(b)) = (left.canonicalize(), right.canonicalize()) {
                return a == b;
            }
            left.file_name() == right.file_name()
                && left.parent().and_then(Path::file_name)
                    == right.parent().and_then(Path::file_name)
        }

        fn reports_path(signals: &[WatchSignal], expected: &Path) -> bool {
            signals.iter().any(|signal| match signal {
                WatchSignal::Changed { paths, .. } => {
                    paths.iter().any(|path| same_path(path, expected))
                }
                WatchSignal::Rescan { .. } => false,
            })
        }

        fn reports_path_on_root(signals: &[WatchSignal], root: &Path, expected: &Path) -> bool {
            signals.iter().any(|signal| match signal {
                WatchSignal::Changed {
                    root: signal_root,
                    paths,
                } => signal_root == root && paths.iter().any(|path| same_path(path, expected)),
                WatchSignal::Rescan { .. } => false,
            })
        }

        fn start_watch(root: &Path) -> (MountWatcher, Receiver<WatchSignal>) {
            let (tx, rx) = mpsc::channel();
            let mut watcher = MountWatcher::new(tx).unwrap();
            watcher.watch(root).unwrap();
            (watcher, rx)
        }

        /// Recreate `path` until the watcher reports it. Notify attaches recursive
        /// watches after emitting the parent CREATE, so the first attempt can race.
        fn create_until_reported(
            rx: &Receiver<WatchSignal>,
            path: &Path,
            mut create: impl FnMut(),
        ) -> Vec<WatchSignal> {
            let deadline = Instant::now() + TIMEOUT;
            let mut all = Vec::new();
            loop {
                create();
                let remaining = deadline.saturating_duration_since(Instant::now());
                let slice = remaining.min(Duration::from_millis(400));
                if slice.is_zero() {
                    return all;
                }
                let batch = drain_until(rx, slice, |s| reports_path(s, path));
                let found = reports_path(&batch, path);
                all.extend(batch);
                if found {
                    return all;
                }
                if path.exists() {
                    if path.is_dir() {
                        let _ = fs::remove_dir_all(path);
                    } else {
                        let _ = fs::remove_file(path);
                    }
                }
            }
        }

        #[test]
        fn create_file_is_reported() {
            let dir = tempdir().unwrap();
            let root = dir.path();
            let (_watcher, rx) = start_watch(root);
            let file = root.join("created.txt");
            fs::write(&file, b"hi").unwrap();
            let signals = drain_until(&rx, TIMEOUT, |s| reports_path(s, &file));
            assert!(
                reports_path(&signals, &file),
                "expected create of {file:?} in {signals:?}"
            );
        }

        #[test]
        fn nested_dir_then_file_is_reported() {
            let dir = tempdir().unwrap();
            let root = dir.path();
            let (_watcher, rx) = start_watch(root);
            let first = root.join("a");
            let after_first = create_until_reported(&rx, &first, || {
                let _ = fs::create_dir(&first);
            });
            assert!(
                reports_path(&after_first, &first),
                "expected new directory {first:?} in {after_first:?}"
            );

            let nested_dir = first.join("b");
            let after_nested = create_until_reported(&rx, &nested_dir, || {
                let _ = fs::create_dir(&nested_dir);
            });
            assert!(
                reports_path(&after_nested, &nested_dir),
                "expected nested directory {nested_dir:?} in {after_nested:?}"
            );

            let file = nested_dir.join("nested.txt");
            let signals = create_until_reported(&rx, &file, || {
                let _ = fs::write(&file, b"hi");
            });
            assert!(
                reports_path(&signals, &file),
                "expected nested file {file:?} in {signals:?}"
            );
        }

        #[test]
        fn write_existing_file_is_reported() {
            let dir = tempdir().unwrap();
            let root = dir.path();
            let file = root.join("edit.txt");
            fs::write(&file, b"v1").unwrap();
            let (_watcher, rx) = start_watch(root);
            let _ = drain_until(&rx, Duration::from_millis(200), |_| false);
            fs::write(&file, b"v2-longer").unwrap();
            let signals = drain_until(&rx, TIMEOUT, |s| reports_path(s, &file));
            assert!(
                reports_path(&signals, &file),
                "expected write of {file:?} in {signals:?}"
            );
        }

        #[test]
        fn delete_is_reported() {
            let dir = tempdir().unwrap();
            let root = dir.path();
            let file = root.join("gone.txt");
            fs::write(&file, b"x").unwrap();
            let (_watcher, rx) = start_watch(root);
            let _ = drain_until(&rx, Duration::from_millis(200), |_| false);
            fs::remove_file(&file).unwrap();
            let signals = drain_until(&rx, TIMEOUT, |s| reports_path(s, &file));
            assert!(
                reports_path(&signals, &file),
                "expected delete of {file:?} in {signals:?}"
            );
        }

        #[test]
        fn rename_reports_new_path() {
            let dir = tempdir().unwrap();
            let root = dir.path();
            let old = root.join("old.txt");
            let new = root.join("new.txt");
            fs::write(&old, b"x").unwrap();
            let (_watcher, rx) = start_watch(root);
            let _ = drain_until(&rx, Duration::from_millis(200), |_| false);
            fs::rename(&old, &new).unwrap();
            let signals = drain_until(&rx, TIMEOUT, |s| reports_path(s, &new));
            assert!(
                reports_path(&signals, &new),
                "expected rename target {new:?} in {signals:?}"
            );
        }

        #[test]
        fn path_outside_root_is_not_reported() {
            let dir = tempdir().unwrap();
            let other = tempdir().unwrap();
            let (_watcher, rx) = start_watch(dir.path());
            let outside = other.path().join("outside.txt");
            fs::write(&outside, b"no").unwrap();
            let signals = drain_until(&rx, Duration::from_millis(400), |_| false);
            assert!(
                !reports_path(&signals, &outside),
                "outside path leaked: {signals:?}"
            );
        }

        #[test]
        fn two_roots_map_to_their_own_events() {
            let first = tempdir().unwrap();
            let second = tempdir().unwrap();
            let (tx, rx) = mpsc::channel();
            let mut watcher = MountWatcher::new(tx).unwrap();
            watcher.watch(first.path()).unwrap();
            watcher.watch(second.path()).unwrap();
            let a = first.path().join("one.txt");
            let b = second.path().join("two.txt");
            fs::write(&a, b"a").unwrap();
            fs::write(&b, b"b").unwrap();
            let signals = drain_until(&rx, TIMEOUT, |s| {
                reports_path_on_root(s, first.path(), &a)
                    && reports_path_on_root(s, second.path(), &b)
            });
            assert!(
                reports_path_on_root(&signals, first.path(), &a),
                "missing first root mapping in {signals:?}"
            );
            assert!(
                reports_path_on_root(&signals, second.path(), &b),
                "missing second root mapping in {signals:?}"
            );
        }

        #[test]
        fn unwatch_stops_signals_for_that_root() {
            let watched = tempdir().unwrap();
            let dropped = tempdir().unwrap();
            let (tx, rx) = mpsc::channel();
            let mut watcher = MountWatcher::new(tx).unwrap();
            watcher.watch(watched.path()).unwrap();
            watcher.watch(dropped.path()).unwrap();
            watcher.unwatch(dropped.path()).unwrap();
            let _ = drain_until(&rx, Duration::from_millis(200), |_| false);

            let ignored = dropped.path().join("ignored.txt");
            let kept = watched.path().join("kept.txt");
            fs::write(&ignored, b"no").unwrap();
            fs::write(&kept, b"yes").unwrap();
            let signals = drain_until(&rx, TIMEOUT, |s| reports_path(s, &kept));
            assert!(
                reports_path(&signals, &kept),
                "expected kept file in {signals:?}"
            );
            assert!(
                !reports_path(&signals, &ignored),
                "unwatched root still reported: {signals:?}"
            );
        }
    }
}
