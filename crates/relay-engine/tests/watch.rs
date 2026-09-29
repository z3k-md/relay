use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use relay_core::{EntryContent, MOUNT_MARKER, ObjectId, TEMP_PREFIX};
use relay_engine::{Engine, ScanOptions, WatchEvent, WatchOptions};
use tempfile::TempDir;

const CONVERGE: Duration = Duration::from_secs(10);

fn ready_dirs() -> (TempDir, TempDir) {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    {
        let mut engine = Engine::init(home.path(), "testdev").unwrap();
        engine.create_space("Personal").unwrap();
        engine
            .add_mount("Personal", "code", mount.path(), &[], &[])
            .unwrap();
    }
    (home, mount)
}

fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn live_paths(home: &Path) -> Vec<String> {
    let engine = Engine::open_read_only(home).unwrap();
    let mut paths: Vec<String> = engine
        .entries("Personal", "code", false)
        .unwrap()
        .into_iter()
        .map(|e| e.key.path.as_str().to_owned())
        .collect();
    paths.sort();
    paths
}

fn live_has(home: &Path, path: &str) -> bool {
    live_paths(home).iter().any(|p| p == path)
}

fn file_object(home: &Path, path: &str) -> Option<ObjectId> {
    let engine = Engine::open_read_only(home).unwrap();
    engine
        .entries("Personal", "code", false)
        .unwrap()
        .into_iter()
        .find(|e| e.key.path.as_str() == path)
        .and_then(|e| e.content.object())
}

struct WatchSession {
    stop: Arc<AtomicBool>,
    events: Receiver<WatchEvent>,
    handle: JoinHandle<Result<(), relay_engine::EngineError>>,
}

fn start_watch(home: &Path, opts: WatchOptions) -> WatchSession {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let home_path = home.to_path_buf();
    let handle = thread::spawn(move || {
        let mut engine = Engine::open(&home_path).unwrap();
        engine.watch(opts, &stop_thread, &mut |event| {
            let _ = tx.send(event.clone());
        })
    });
    WatchSession {
        stop,
        events: rx,
        handle,
    }
}

fn wait_started(session: &WatchSession) {
    assert!(
        wait_until(CONVERGE, || {
            session
                .events
                .try_iter()
                .any(|e| matches!(e, WatchEvent::Started { .. }))
        }),
        "watch did not start"
    );
}

fn drain_events(session: &WatchSession) {
    while session.events.try_recv().is_ok() {}
}

fn stop_watch(session: WatchSession) {
    session.stop.store(true, Ordering::SeqCst);
    let stopped = wait_until(CONVERGE, || {
        session
            .events
            .try_iter()
            .any(|e| matches!(e, WatchEvent::Stopped))
    });
    let result = session.handle.join().expect("watch thread panicked");
    result.expect("watch returned error");
    assert!(stopped, "did not observe Stopped");
}

fn watch_opts(debounce_ms: u64, interval: Duration, use_watcher: bool) -> WatchOptions {
    WatchOptions {
        debounce: Duration::from_millis(debounce_ms),
        max_batch_delay: Duration::from_millis(500),
        full_scan_interval: interval,
        use_watcher,
        max_dirty_paths: 10_000,
    }
}

fn disk_live(root: &Path) -> BTreeMap<String, DiskEnt> {
    let mut out = BTreeMap::new();
    walk_disk(root, root, &mut out);
    out
}

#[derive(Debug, PartialEq, Eq)]
enum DiskEnt {
    File { size: u64, id: ObjectId },
    Dir,
    Link { target: String },
}

fn walk_disk(root: &Path, dir: &Path, out: &mut BTreeMap<String, DiskEnt>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let entry = entry.unwrap();
        let os_path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == MOUNT_MARKER || name.starts_with(TEMP_PREFIX) {
            continue;
        }
        let Ok(logical) = relay_fs::to_logical_path(root, &os_path) else {
            continue;
        };
        let Ok(meta) = fs::symlink_metadata(&os_path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&os_path)
                .ok()
                .and_then(|p| p.to_str().map(ToOwned::to_owned))
                .unwrap_or_default();
            out.insert(logical.to_string(), DiskEnt::Link { target });
        } else if meta.is_dir() {
            out.insert(logical.to_string(), DiskEnt::Dir);
            walk_disk(root, &os_path, out);
        } else if meta.is_file() {
            let bytes = fs::read(&os_path).unwrap_or_default();
            out.insert(
                logical.to_string(),
                DiskEnt::File {
                    size: bytes.len() as u64,
                    id: ObjectId::of(&bytes),
                },
            );
        }
    }
}

fn index_live(home: &Path) -> BTreeMap<String, DiskEnt> {
    let engine = Engine::open_read_only(home).unwrap();
    engine
        .entries("Personal", "code", false)
        .unwrap()
        .into_iter()
        .map(|e| {
            let ent = match &e.content {
                EntryContent::File { object, size, .. } => DiskEnt::File {
                    size: *size,
                    id: *object,
                },
                EntryContent::Directory => DiskEnt::Dir,
                EntryContent::Symlink { target } => DiskEnt::Link {
                    target: target.clone(),
                },
                EntryContent::Deleted => panic!("live deleted"),
            };
            (e.key.path.to_string(), ent)
        })
        .collect()
}

fn wait_index_matches(home: &Path, mount: &Path) -> bool {
    wait_until(CONVERGE, || index_live(home) == disk_live(mount))
}

#[test]
fn watch_create_modify_delete_then_stop() {
    let (home, mount) = ready_dirs();
    let session = start_watch(home.path(), watch_opts(50, Duration::from_secs(600), true));
    wait_started(&session);

    fs::write(mount.path().join("a.txt"), b"one").unwrap();
    assert!(
        wait_until(CONVERGE, || live_has(home.path(), "a.txt")),
        "create did not converge: {:?}",
        live_paths(home.path())
    );

    fs::write(mount.path().join("a.txt"), b"two-longer").unwrap();
    assert!(
        wait_until(CONVERGE, || {
            file_object(home.path(), "a.txt") == Some(ObjectId::of(b"two-longer"))
        }),
        "modify did not converge"
    );

    fs::remove_file(mount.path().join("a.txt")).unwrap();
    assert!(
        wait_until(CONVERGE, || !live_has(home.path(), "a.txt")),
        "delete did not converge: {:?}",
        live_paths(home.path())
    );

    stop_watch(session);
}

#[test]
fn watch_editor_save_stress() {
    let (home, mount) = ready_dirs();
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::write(mount.path().join(name), b"seed").unwrap();
    }
    let session = start_watch(home.path(), watch_opts(50, Duration::from_secs(600), true));
    wait_started(&session);
    assert!(
        wait_until(CONVERGE, || {
            live_has(home.path(), "a.txt")
                && live_has(home.path(), "b.txt")
                && live_has(home.path(), "c.txt")
        }),
        "initial files missing: {:?}",
        live_paths(home.path())
    );

    let names = ["a.txt", "b.txt", "c.txt"];
    for i in 0..150 {
        let name = names[i % 3];
        let dest = mount.path().join(name);
        let payload = format!("v{i}-{}", name).into_bytes();
        if i % 2 == 0 {
            let tmp = mount.path().join(format!(".{name}.tmp"));
            fs::write(&tmp, &payload).unwrap();
            fs::rename(&tmp, &dest).unwrap();
        } else {
            let mut file = File::create(&dest).unwrap();
            file.set_len(0).unwrap();
            file.write_all(&payload).unwrap();
        }
        thread::sleep(Duration::from_millis(u64::from((i % 6) as u8)));
    }

    assert!(
        wait_index_matches(home.path(), mount.path()),
        "index did not catch up before stop\nindex={:?}\ndisk={:?}",
        index_live(home.path()),
        disk_live(mount.path())
    );
    assert!(
        wait_until(Duration::from_secs(2), || {
            index_live(home.path()) == disk_live(mount.path())
        }),
        "lost quiescence"
    );

    stop_watch(session);

    let mut engine = Engine::open(home.path()).unwrap();
    let deadline = Instant::now() + CONVERGE;
    loop {
        let dry = engine
            .scan(
                "Personal",
                "code",
                ScanOptions {
                    dry_run: true,
                    ..ScanOptions::default()
                },
            )
            .unwrap();
        if dry.created == 0 && dry.modified == 0 && dry.deleted == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "dry-run still dirty after stop: {dry:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert_index_eq(&engine, mount.path());
    let verify = engine.verify_objects().unwrap();
    assert!(
        verify.missing.is_empty() && verify.corrupt.is_empty(),
        "{verify:?}"
    );
}

fn assert_index_eq(engine: &Engine, root: &Path) {
    let disk = disk_live(root);
    let live = engine.entries("Personal", "code", false).unwrap();
    let index: BTreeMap<String, DiskEnt> = live
        .iter()
        .map(|e| {
            let ent = match &e.content {
                EntryContent::File { object, size, .. } => DiskEnt::File {
                    size: *size,
                    id: *object,
                },
                EntryContent::Directory => DiskEnt::Dir,
                EntryContent::Symlink { target } => DiskEnt::Link {
                    target: target.clone(),
                },
                EntryContent::Deleted => panic!("live deleted"),
            };
            (e.key.path.to_string(), ent)
        })
        .collect();
    assert_eq!(index, disk);
}

#[test]
fn watch_poll_heals_and_restart_picks_up_offline_changes() {
    let (home, mount) = ready_dirs();
    fs::write(mount.path().join("seed.txt"), b"s").unwrap();
    let session = start_watch(
        home.path(),
        watch_opts(50, Duration::from_millis(300), false),
    );
    wait_started(&session);
    assert!(
        wait_until(CONVERGE, || live_has(home.path(), "seed.txt")),
        "poll initial scan missed seed"
    );

    fs::write(mount.path().join("later.txt"), b"late").unwrap();
    assert!(
        wait_until(CONVERGE, || live_has(home.path(), "later.txt")),
        "polling mode did not heal later.txt: {:?}",
        live_paths(home.path())
    );
    stop_watch(session);

    fs::write(mount.path().join("offline.txt"), b"off").unwrap();
    fs::remove_file(mount.path().join("seed.txt")).unwrap();

    let session = start_watch(
        home.path(),
        watch_opts(50, Duration::from_millis(300), false),
    );
    wait_started(&session);
    assert!(
        wait_until(CONVERGE, || {
            live_has(home.path(), "offline.txt") && !live_has(home.path(), "seed.txt")
        }),
        "restart did not pick up offline changes: {:?}",
        live_paths(home.path())
    );
    stop_watch(session);
}

#[test]
fn watch_restart_reconstructs_same_index() {
    let (home, mount) = ready_dirs();
    fs::write(mount.path().join("doc.txt"), b"hello").unwrap();
    let session = start_watch(home.path(), watch_opts(50, Duration::from_secs(600), true));
    wait_started(&session);
    assert!(
        wait_until(CONVERGE, || live_has(home.path(), "doc.txt")),
        "doc.txt never appeared"
    );
    assert!(wait_index_matches(home.path(), mount.path()));
    let before = Engine::open_read_only(home.path())
        .unwrap()
        .entries("Personal", "code", true)
        .unwrap();
    stop_watch(session);

    let mut engine = Engine::open(home.path()).unwrap();
    let dry = engine
        .scan(
            "Personal",
            "code",
            ScanOptions {
                dry_run: true,
                ..ScanOptions::default()
            },
        )
        .unwrap();
    assert_eq!(dry.created, 0, "{dry:?}");
    assert_eq!(dry.modified, 0, "{dry:?}");
    assert_eq!(dry.deleted, 0, "{dry:?}");
    let after = engine.entries("Personal", "code", true).unwrap();
    assert_eq!(before, after);
}

#[test]
fn watch_relayignore_deselects_instead_of_delete() {
    let (home, mount) = ready_dirs();
    fs::write(mount.path().join("keep.txt"), b"yes").unwrap();
    fs::write(mount.path().join("noise.txt"), b"no").unwrap();
    let session = start_watch(home.path(), watch_opts(50, Duration::from_secs(600), true));
    wait_started(&session);
    assert!(
        wait_until(CONVERGE, || {
            live_has(home.path(), "keep.txt") && live_has(home.path(), "noise.txt")
        }),
        "initial files missing"
    );
    drain_events(&session);

    fs::write(mount.path().join(".relayignore"), "noise.txt\n").unwrap();
    assert!(
        wait_until(CONVERGE, || {
            let engine = Engine::open_read_only(home.path()).unwrap();
            let live = engine.entries("Personal", "code", false).unwrap();
            live.iter().any(|e| e.key.path.as_str() == "noise.txt")
                && live.iter().any(|e| e.key.path.as_str() == "keep.txt")
                && live.iter().any(|e| e.key.path.as_str() == ".relayignore")
        }),
        "ignored path was dropped: {:?}",
        live_paths(home.path())
    );

    let saw_full = wait_until(CONVERGE, || {
        session.events.try_iter().any(|e| match e {
            WatchEvent::Scanned { full, .. } => full,
            _ => false,
        })
    });
    assert!(
        saw_full,
        "changing .relayignore did not trigger a full scan"
    );
    stop_watch(session);
}
