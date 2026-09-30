use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use relay_engine::{
    Engine, EngineConfig, EngineError, EntryContent, LogicalPath, ManualClock, ObjectStore,
    ScanOptions, Sequence, VectorOrdering,
};
use tempfile::TempDir;

const CLOCK_START: i64 = 1_700_000_000_000;

fn new_home() -> TempDir {
    TempDir::new().unwrap()
}

fn init_engine(home: &Path) -> Engine {
    Engine::init(home, "testdev")
        .unwrap()
        .with_clock(Arc::new(ManualClock::new(CLOCK_START)))
        .with_config(EngineConfig {
            racy_window: Duration::ZERO,
        })
}

fn ready(home: &Path, mount: &Path) -> Engine {
    let mut engine = init_engine(home);
    engine.create_space("Personal").unwrap();
    engine
        .add_mount("Personal", "code", mount, &[], &[])
        .unwrap();
    engine
}

fn scan(engine: &mut Engine) -> relay_engine::ScanReport {
    engine
        .scan("Personal", "code", ScanOptions::default())
        .unwrap()
}

fn lp(path: &str) -> LogicalPath {
    LogicalPath::new(path).unwrap()
}

fn last_seq(engine: &Engine) -> Sequence {
    engine.status().unwrap().last_sequence
}

fn object_id(content: &EntryContent) -> relay_engine::ObjectId {
    content.object().expect("file object")
}

#[test]
fn phase0_success_path() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());

    fs::create_dir_all(mount.path().join("src")).unwrap();
    fs::write(mount.path().join("src/a.txt"), b"alpha").unwrap();
    fs::write(mount.path().join("b.txt"), b"bravo").unwrap();

    let report = scan(&mut engine);
    assert_eq!(report.created, 3, "{report:?}");
    assert_eq!(report.modified, 0);
    assert_eq!(report.deleted, 0);

    let store = ObjectStore::open(home.path().join("store")).unwrap();
    let entries = engine.entries("Personal", "code", false).unwrap();
    let files: Vec<_> = entries
        .iter()
        .filter(|e| matches!(e.content, EntryContent::File { .. }))
        .collect();
    assert_eq!(files.len(), 2);
    for entry in &files {
        let id = object_id(&entry.content);
        assert!(store.contains(&id));
    }

    fs::write(mount.path().join("src/a.txt"), b"alpha-2").unwrap();
    fs::write(mount.path().join("c.txt"), b"charlie").unwrap();
    fs::remove_file(mount.path().join("b.txt")).unwrap();

    let report = scan(&mut engine);
    assert_eq!(report.created, 1, "{report:?}");
    assert_eq!(report.modified, 1);
    assert_eq!(report.deleted, 1);

    let seq_after = last_seq(&engine);
    let report = scan(&mut engine);
    assert_eq!(report.created, 0);
    assert_eq!(report.modified, 0);
    assert_eq!(report.deleted, 0);
    assert!(report.unchanged >= 3);
    assert_eq!(last_seq(&engine), seq_after);

    let history = engine
        .history("Personal", "code", &lp("src/a.txt"))
        .unwrap();
    assert_eq!(history.len(), 2);
    assert!(history[0].sequence < history[1].sequence);
    match &history[0].content {
        EntryContent::File { size, .. } => assert_eq!(*size, 5),
        other => panic!("{other:?}"),
    }
    match &history[1].content {
        EntryContent::File { size, .. } => assert_eq!(*size, 7),
        other => panic!("{other:?}"),
    }

    let live = engine.entries("Personal", "code", false).unwrap();
    assert!(!live.iter().any(|e| e.key.path.as_str() == "b.txt"));
    let all = engine.entries("Personal", "code", true).unwrap();
    let tomb = all.iter().find(|e| e.key.path.as_str() == "b.txt").unwrap();
    assert!(tomb.is_deleted());
}

#[cfg(unix)]
#[test]
fn in_place_overwrite_preserving_mtime_is_detected_via_ctime() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    let path = mount.path().join("same.txt");
    fs::write(&path, b"AAAAA").unwrap();
    let original_mtime = fs::metadata(&path).unwrap().modified().unwrap();
    scan(&mut engine);

    fs::write(&path, b"BBBBB").unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(original_mtime)
        .unwrap();

    let report = scan(&mut engine);
    assert_eq!(report.modified, 1, "{report:?}");
    let entry = engine
        .entries("Personal", "code", false)
        .unwrap()
        .into_iter()
        .find(|e| e.key.path.as_str() == "same.txt")
        .unwrap();
    assert_eq!(
        entry.content.object(),
        Some(relay_engine::ObjectId::of(b"BBBBB"))
    );
}

#[test]
fn editor_atomic_save_is_one_modify() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    fs::write(mount.path().join("a.lua"), b"v1").unwrap();
    scan(&mut engine);

    fs::write(mount.path().join(".a.lua.tmp"), b"v2").unwrap();
    fs::rename(mount.path().join(".a.lua.tmp"), mount.path().join("a.lua")).unwrap();

    let report = scan(&mut engine);
    assert_eq!(report.modified, 1, "{report:?}");
    assert_eq!(report.created, 0);
    assert_eq!(report.deleted, 0);
}

#[test]
fn touch_is_stat_only() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    let path = mount.path().join("note.txt");
    fs::write(&path, b"same").unwrap();
    scan(&mut engine);
    let seq = last_seq(&engine);

    let file = File::options().write(true).open(&path).unwrap();
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000))
        .unwrap();
    drop(file);

    let report = scan(&mut engine);
    assert_eq!(report.stat_only, 1, "{report:?}");
    assert_eq!(report.modified, 0);
    assert_eq!(report.created, 0);
    assert_eq!(last_seq(&engine), seq);
}

#[cfg(unix)]
#[test]
fn exec_bit_flip_is_modified() {
    use std::os::unix::fs::PermissionsExt;

    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    let path = mount.path().join("tool.sh");
    fs::write(&path, b"#!/bin/sh\n").unwrap();
    scan(&mut engine);

    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();

    let report = scan(&mut engine);
    assert_eq!(report.modified, 1, "{report:?}");
    let entry = engine
        .entries("Personal", "code", false)
        .unwrap()
        .into_iter()
        .find(|e| e.key.path.as_str() == "tool.sh")
        .unwrap();
    match entry.content {
        EntryContent::File { executable, .. } => assert!(executable),
        other => panic!("{other:?}"),
    }
}

#[test]
fn identical_files_share_one_object() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    fs::write(mount.path().join("a.txt"), b"same-bytes").unwrap();
    fs::write(mount.path().join("b.txt"), b"same-bytes").unwrap();
    scan(&mut engine);

    let store = ObjectStore::open(home.path().join("store")).unwrap();
    assert_eq!(store.list().unwrap().len(), 1);
    let entries = engine.entries("Personal", "code", false).unwrap();
    let ids: Vec<_> = entries.iter().filter_map(|e| e.content.object()).collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], ids[1]);
}

#[test]
fn missing_marker_refuses_scan() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    fs::write(mount.path().join("a.txt"), b"x").unwrap();
    scan(&mut engine);
    let seq = last_seq(&engine);

    fs::remove_file(mount.path().join(".relay-mount")).unwrap();
    let err = engine
        .scan("Personal", "code", ScanOptions::default())
        .unwrap_err();
    assert!(matches!(err, EngineError::Fs(_)), "{err}");
    assert_eq!(last_seq(&engine), seq);
    assert_eq!(
        engine
            .entries("Personal", "code", false)
            .unwrap()
            .iter()
            .filter(|e| e.content.object().is_some())
            .count(),
        1
    );
}

#[test]
fn missing_mount_root_refuses_scan() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    fs::write(mount.path().join("a.txt"), b"x").unwrap();
    scan(&mut engine);
    let seq = last_seq(&engine);

    let gone = mount.path().parent().unwrap().join("renamed-mount");
    fs::rename(mount.path(), &gone).unwrap();
    let err = engine
        .scan("Personal", "code", ScanOptions::default())
        .unwrap_err();
    assert!(matches!(err, EngineError::Fs(_)), "{err}");
    assert_eq!(last_seq(&engine), seq);
    let _ = fs::rename(&gone, mount.path());
}

#[test]
fn mass_delete_guard() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    for i in 0..30 {
        fs::write(mount.path().join(format!("f{i}.txt")), b"x").unwrap();
    }
    scan(&mut engine);
    let seq = last_seq(&engine);

    for i in 0..30 {
        fs::remove_file(mount.path().join(format!("f{i}.txt"))).unwrap();
    }
    let err = engine
        .scan("Personal", "code", ScanOptions::default())
        .unwrap_err();
    match err {
        EngineError::MassDeleteRefused { deletions, live } => {
            assert_eq!(deletions, 30);
            assert_eq!(live, 30);
        }
        other => panic!("{other}"),
    }
    assert_eq!(last_seq(&engine), seq);
    assert_eq!(
        engine
            .entries("Personal", "code", false)
            .unwrap()
            .iter()
            .filter(|e| e.content.object().is_some())
            .count(),
        30
    );

    let report = engine
        .scan(
            "Personal",
            "code",
            ScanOptions {
                allow_mass_delete: true,
                ..ScanOptions::default()
            },
        )
        .unwrap();
    assert_eq!(report.deleted, 30);
    assert_eq!(engine.entries("Personal", "code", false).unwrap().len(), 0);
    assert_eq!(
        engine
            .entries("Personal", "code", true)
            .unwrap()
            .iter()
            .filter(|e| e.is_deleted())
            .count(),
        30
    );
}

#[test]
fn empty_scan_of_small_mount_is_refused() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::write(mount.path().join(name), b"x").unwrap();
    }
    scan(&mut engine);
    let seq = last_seq(&engine);
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::remove_file(mount.path().join(name)).unwrap();
    }
    let err = engine
        .scan("Personal", "code", ScanOptions::default())
        .unwrap_err();
    assert!(
        matches!(
            err,
            EngineError::MassDeleteRefused {
                deletions: 3,
                live: 3
            }
        ),
        "{err}"
    );
    assert_eq!(last_seq(&engine), seq);
}

#[test]
fn rule_change_deselects_instead_of_tombstone() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    fs::create_dir_all(mount.path().join("node_modules/pkg")).unwrap();
    fs::write(mount.path().join("node_modules/pkg/x.js"), b"mod").unwrap();
    fs::write(mount.path().join("keep.txt"), b"ok").unwrap();
    scan(&mut engine);

    fs::write(mount.path().join(".relayignore"), "**/node_modules/**\n").unwrap();
    let report = scan(&mut engine);
    assert_eq!(report.deleted, 0, "{report:?}");
    assert!(
        report
            .deselected
            .iter()
            .any(|p| p.as_str() == "node_modules" || p.as_str().starts_with("node_modules/")),
        "{report:?}"
    );
    let live = engine.entries("Personal", "code", false).unwrap();
    assert!(
        live.iter()
            .any(|e| e.key.path.as_str().contains("node_modules"))
    );
}

#[cfg(unix)]
#[test]
fn unreadable_directory_is_protected() {
    use std::os::unix::fs::PermissionsExt;

    if unreadable_dirs_still_readable() {
        eprintln!(
            "skipping unreadable_directory_is_protected: running with permissions that bypass chmod"
        );
        return;
    }

    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    let secret = mount.path().join("secret");
    fs::create_dir_all(&secret).unwrap();
    fs::write(secret.join("hidden.txt"), b"nope").unwrap();
    scan(&mut engine);
    let seq = last_seq(&engine);

    let mut perms = fs::metadata(&secret).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&secret, perms).unwrap();

    let report = match engine.scan("Personal", "code", ScanOptions::default()) {
        Ok(report) => report,
        Err(err) => {
            let _ = fs::set_permissions(&secret, fs::Permissions::from_mode(0o755));
            panic!("{err}");
        }
    };
    let _ = fs::set_permissions(&secret, fs::Permissions::from_mode(0o755));

    assert_eq!(report.deleted, 0, "{report:?}");
    assert!(report.protected >= 1, "{report:?}");
    assert_eq!(last_seq(&engine), seq);
    let live = engine.entries("Personal", "code", false).unwrap();
    assert!(
        live.iter()
            .any(|e| e.key.path.as_str() == "secret/hidden.txt")
    );
}

#[cfg(unix)]
#[test]
fn unreadable_file_is_a_warning_not_a_failed_scan() {
    use std::os::unix::fs::PermissionsExt;

    if unreadable_dirs_still_readable() {
        eprintln!(
            "skipping unreadable_file_is_a_warning_not_a_failed_scan: running with permissions that bypass chmod"
        );
        return;
    }

    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    let locked = mount.path().join("locked.txt");
    fs::write(&locked, b"v1").unwrap();
    fs::write(mount.path().join("other.txt"), b"x").unwrap();
    scan(&mut engine);

    fs::write(&locked, b"v2 is longer").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    fs::write(mount.path().join("new.txt"), b"new").unwrap();
    let result = engine.scan("Personal", "code", ScanOptions::default());
    let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o644));
    let report = result.unwrap();

    assert_eq!(report.created, 1, "{report:?}");
    assert_eq!(report.deleted, 0, "{report:?}");
    assert_eq!(report.unstable, vec![lp("locked.txt")]);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.message.contains("locked.txt"))
    );

    let report = scan(&mut engine);
    assert_eq!(report.modified, 1, "{report:?}");
}

#[cfg(unix)]
fn unreadable_dirs_still_readable() -> bool {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let secret = dir.path().join("s");
    fs::create_dir(&secret).unwrap();
    let mut perms = fs::metadata(&secret).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&secret, perms).unwrap();
    let readable = fs::read_dir(&secret).is_ok();
    let _ = fs::set_permissions(&secret, fs::Permissions::from_mode(0o755));
    readable
}

#[cfg(unix)]
#[test]
fn fifo_replacing_regular_file_is_protected() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    let path = mount.path().join("pipe");
    fs::write(&path, b"regular").unwrap();
    scan(&mut engine);

    fs::remove_file(&path).unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());

    let report = scan(&mut engine);
    assert_eq!(report.deleted, 0, "{report:?}");
    assert!(report.protected >= 1, "{report:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn restore_writes_to_existing_nfd_on_disk_name() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    let nfd = mount.path().join("cafe\u{301}.txt");
    let nfc = mount.path().join("caf\u{e9}.txt");
    fs::write(&nfd, b"version-one").unwrap();
    scan(&mut engine);
    fs::write(&nfd, b"version-two").unwrap();
    scan(&mut engine);

    let history = engine
        .history("Personal", "code", &lp("caf\u{e9}.txt"))
        .unwrap();
    assert_eq!(history.len(), 2);
    engine
        .restore(
            "Personal",
            "code",
            &lp("caf\u{e9}.txt"),
            history[0].sequence,
        )
        .unwrap();

    assert_eq!(fs::read(&nfd).unwrap(), b"version-one");
    assert!(!nfc.exists());
}

#[test]
fn overlapping_mounts_rejected_both_orders() {
    let home = new_home();
    let parent = new_home();
    let child = parent.path().join("child");
    fs::create_dir_all(&child).unwrap();
    let mut engine = init_engine(home.path());
    engine.create_space("S").unwrap();
    engine
        .add_mount("S", "parent", parent.path(), &[], &[])
        .unwrap();
    let err = engine
        .add_mount("S", "child", &child, &[], &[])
        .unwrap_err();
    assert!(matches!(err, EngineError::OverlappingMount { .. }), "{err}");

    let home2 = new_home();
    let parent2 = new_home();
    let child2 = parent2.path().join("child");
    fs::create_dir_all(&child2).unwrap();
    let mut engine = init_engine(home2.path());
    engine.create_space("S").unwrap();
    engine.add_mount("S", "child", &child2, &[], &[]).unwrap();
    let err = engine
        .add_mount("S", "parent", parent2.path(), &[], &[])
        .unwrap_err();
    assert!(matches!(err, EngineError::OverlappingMount { .. }), "{err}");
}

#[test]
fn mount_containing_home_rejected() {
    let home = new_home();
    let mut engine = init_engine(home.path());
    engine.create_space("S").unwrap();
    let err = engine
        .add_mount("S", "home", home.path(), &[], &[])
        .unwrap_err();
    assert!(matches!(err, EngineError::OverlapsRelayHome), "{err}");

    if let Some(parent) = home.path().parent() {
        let err = engine.add_mount("S", "tmp", parent, &[], &[]).unwrap_err();
        assert!(matches!(err, EngineError::OverlapsRelayHome), "{err}");
    }
}

#[test]
fn existing_marker_rejected() {
    let home = new_home();
    let mount = new_home();
    fs::write(mount.path().join(".relay-mount"), "already").unwrap();
    let mut engine = init_engine(home.path());
    engine.create_space("S").unwrap();
    let err = engine
        .add_mount("S", "code", mount.path(), &[], &[])
        .unwrap_err();
    assert!(
        matches!(err, EngineError::MountAlreadyClaimed { .. }),
        "{err}"
    );
}

#[test]
fn leftover_marker_from_this_device_is_adopted() {
    let home = new_home();
    let mount = new_home();
    let mut engine = init_engine(home.path());
    engine.create_space("S").unwrap();
    relay_fs::MountMarker {
        space: relay_core::SpaceId::new(),
        mount: relay_core::MountId::new(),
        created_by: engine.device().id,
    }
    .write(mount.path())
    .unwrap();

    engine
        .add_mount("S", "code", mount.path(), &[], &[])
        .unwrap();
    let mounts = engine.mounts(None).unwrap();
    assert_eq!(mounts.len(), 1);
    let marker = relay_fs::MountMarker::read(mount.path()).unwrap();
    assert_eq!(marker.created_by, engine.device().id);
    assert_eq!(marker.mount, mounts[0].1.mount.id);
}

#[test]
fn leftover_marker_for_known_mount_is_rejected() {
    let home = new_home();
    let first = new_home();
    let leftover = new_home();
    let mut engine = init_engine(home.path());
    engine.create_space("S").unwrap();
    engine
        .add_mount("S", "code", first.path(), &[], &[])
        .unwrap();
    let existing = engine.mounts(None).unwrap()[0].1.mount.clone();
    relay_fs::MountMarker {
        space: existing.space,
        mount: existing.id,
        created_by: engine.device().id,
    }
    .write(leftover.path())
    .unwrap();

    let err = engine
        .add_mount("S", "other", leftover.path(), &[], &[])
        .unwrap_err();
    assert!(
        matches!(err, EngineError::MountAlreadyClaimed { .. }),
        "{err}"
    );
}

#[test]
fn restore_older_version_and_destination_changed() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    let path = mount.path().join("doc.txt");
    fs::write(&path, b"version-one").unwrap();
    scan(&mut engine);
    fs::write(&path, b"version-two").unwrap();
    scan(&mut engine);

    let history = engine.history("Personal", "code", &lp("doc.txt")).unwrap();
    assert_eq!(history.len(), 2);
    let old_seq = history[0].sequence;
    let before = engine
        .entries("Personal", "code", false)
        .unwrap()
        .into_iter()
        .find(|e| e.key.path.as_str() == "doc.txt")
        .unwrap();

    let restored = engine
        .restore("Personal", "code", &lp("doc.txt"), old_seq)
        .unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"version-one");
    assert_eq!(
        restored.vector.compare(&before.vector),
        VectorOrdering::Dominates
    );
    let history = engine.history("Personal", "code", &lp("doc.txt")).unwrap();
    assert_eq!(history.len(), 3);

    fs::write(&path, b"changed-on-disk").unwrap();
    let err = engine
        .restore("Personal", "code", &lp("doc.txt"), old_seq)
        .unwrap_err();
    assert!(matches!(err, EngineError::DestinationChanged(_)), "{err}");
    assert_eq!(fs::read(&path).unwrap(), b"changed-on-disk");
}

#[test]
fn gc_and_verify() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    fs::write(mount.path().join("doc.txt"), b"first").unwrap();
    scan(&mut engine);
    fs::write(mount.path().join("doc.txt"), b"second").unwrap();
    scan(&mut engine);

    let store = ObjectStore::open(home.path().join("store")).unwrap();
    let old = relay_engine::ObjectId::of(b"first");
    let new = relay_engine::ObjectId::of(b"second");
    assert!(store.contains(&old));
    assert!(store.contains(&new));

    let orphan = store.put_bytes(b"orphan-bytes").unwrap();
    let report = engine.gc(Duration::ZERO).unwrap();
    assert!(report.removed >= 1);
    assert!(!store.contains(&orphan));
    assert!(store.contains(&old));
    assert!(store.contains(&new));

    fs::write(store.path_for(&new), b"corrupted").unwrap();
    let verify = engine.verify_objects().unwrap();
    assert!(verify.corrupt.contains(&new), "{verify:?}");
}

#[test]
fn sequences_increase_across_restart() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    fs::write(mount.path().join("a.txt"), b"v1").unwrap();
    scan(&mut engine);
    let seq = last_seq(&engine);
    assert!(seq.0 >= 1);

    drop(engine);
    let mut engine = Engine::open(home.path())
        .unwrap()
        .with_clock(Arc::new(ManualClock::new(CLOCK_START + 60_000)))
        .with_config(EngineConfig {
            racy_window: Duration::ZERO,
        });
    fs::write(mount.path().join("a.txt"), b"v2").unwrap();
    scan(&mut engine);
    let later = last_seq(&engine);
    assert!(later > seq, "{later:?} vs {seq:?}");
}

#[test]
fn init_and_open_guards() {
    let home = new_home();
    let missing = new_home();
    assert!(matches!(
        Engine::open(missing.path()),
        Err(EngineError::NotInitialized)
    ));
    Engine::init(home.path(), "one").unwrap();
    assert!(matches!(
        Engine::init(home.path(), "two"),
        Err(EngineError::AlreadyInitialized)
    ));
    Engine::open(home.path()).unwrap();
}

#[test]
fn init_writes_identity_and_open_requires_matching_key() {
    let home = new_home();
    let engine = Engine::init(home.path(), "one").unwrap();
    let id = engine.device().id;
    let loaded = engine.load_identity().unwrap();
    assert_eq!(loaded.device_id(), id);
    drop(engine);

    let reopened = Engine::open(home.path()).unwrap();
    assert_eq!(reopened.device().id, id);
    drop(reopened);

    std::fs::remove_dir_all(home.path().join("identity")).unwrap();
    assert!(matches!(
        Engine::open(home.path()),
        Err(EngineError::StaleIdentity { .. })
    ));
}

#[test]
fn second_writer_is_busy_reader_works_and_lock_releases_on_drop() {
    let home = new_home();
    let engine = Engine::init(home.path(), "one").unwrap();

    let err = match Engine::open(home.path()) {
        Err(err) => err,
        Ok(_) => panic!("expected second writer to be Busy"),
    };
    assert!(
        matches!(err, EngineError::Busy { ref home } if home == engine.home()),
        "{err}"
    );
    assert_eq!(
        err.to_string(),
        format!("another relay process is using {}", engine.home().display())
    );

    let reader = Engine::open_read_only(home.path()).unwrap();
    assert_eq!(reader.device().name, "one");
    assert!(reader.spaces().unwrap().is_empty());
    assert!(matches!(
        Engine::open_read_only(home.path())
            .unwrap()
            .create_space("nope")
            .unwrap_err(),
        EngineError::ReadOnly
    ));

    drop(engine);
    Engine::open(home.path()).unwrap();
}

#[test]
fn read_only_engine_rejects_create_space() {
    let home = new_home();
    Engine::init(home.path(), "one").unwrap();
    let err = Engine::open_read_only(home.path())
        .unwrap()
        .create_space("Personal")
        .unwrap_err();
    assert!(matches!(err, EngineError::ReadOnly), "{err}");
}

#[test]
fn racy_clean_just_written_file_has_no_stat_and_detects_same_mtime_rewrite() {
    let home = new_home();
    let mount = new_home();
    let mut engine = Engine::init(home.path(), "testdev")
        .unwrap()
        .with_clock(Arc::new(ManualClock::new(CLOCK_START)));
    engine.create_space("Personal").unwrap();
    engine
        .add_mount("Personal", "code", mount.path(), &[], &[])
        .unwrap();

    let path = mount.path().join("racy.txt");
    fs::write(&path, b"aaaa").unwrap();
    let original_mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let report = scan(&mut engine);
    assert_eq!(report.created, 1, "{report:?}");
    let entry = engine
        .entries("Personal", "code", false)
        .unwrap()
        .into_iter()
        .find(|e| e.key.path.as_str() == "racy.txt")
        .unwrap();
    assert_eq!(entry.stat, None);

    fs::write(&path, b"bbbb").unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(original_mtime)
        .unwrap();

    let report = scan(&mut engine);
    assert_eq!(report.modified, 1, "{report:?}");
}

#[test]
fn old_mtime_stores_a_real_stat() {
    let home = new_home();
    let mount = new_home();
    let mut engine = Engine::init(home.path(), "testdev")
        .unwrap()
        .with_clock(Arc::new(ManualClock::new(CLOCK_START)));
    engine.create_space("Personal").unwrap();
    engine
        .add_mount("Personal", "code", mount.path(), &[], &[])
        .unwrap();

    let path = mount.path().join("old.txt");
    fs::write(&path, b"old").unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();

    scan(&mut engine);
    let entry = engine
        .entries("Personal", "code", false)
        .unwrap()
        .into_iter()
        .find(|e| e.key.path.as_str() == "old.txt")
        .unwrap();
    assert!(entry.stat.is_some(), "{entry:?}");
}

#[test]
fn restore_after_racy_scan_and_unscanned_edit() {
    let home = new_home();
    let mount = new_home();
    let mut engine = Engine::init(home.path(), "testdev")
        .unwrap()
        .with_clock(Arc::new(ManualClock::new(CLOCK_START)));
    engine.create_space("Personal").unwrap();
    engine
        .add_mount("Personal", "code", mount.path(), &[], &[])
        .unwrap();

    let path = mount.path().join("doc.txt");
    fs::write(&path, b"version-one").unwrap();
    scan(&mut engine);
    let old_seq = engine.history("Personal", "code", &lp("doc.txt")).unwrap()[0].sequence;
    fs::write(&path, b"version-two").unwrap();
    scan(&mut engine);
    let current = engine
        .entries("Personal", "code", false)
        .unwrap()
        .into_iter()
        .find(|e| e.key.path.as_str() == "doc.txt")
        .unwrap();
    assert_eq!(current.stat, None);

    let restored = engine
        .restore("Personal", "code", &lp("doc.txt"), old_seq)
        .unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"version-one");
    assert!(restored.sequence > current.sequence);

    fs::write(&path, b"changed-on-disk").unwrap();
    let err = engine
        .restore("Personal", "code", &lp("doc.txt"), old_seq)
        .unwrap_err();
    assert!(matches!(err, EngineError::DestinationChanged(_)), "{err}");
    assert_eq!(fs::read(&path).unwrap(), b"changed-on-disk");
}

#[test]
fn dry_run_matches_real_scan_and_writes_nothing() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    fs::write(mount.path().join("a.txt"), b"alpha").unwrap();
    fs::create_dir_all(mount.path().join("src")).unwrap();
    fs::write(mount.path().join("src/b.txt"), b"bravo").unwrap();

    let seq_before = last_seq(&engine);
    let store_before = ObjectStore::open(home.path().join("store"))
        .unwrap()
        .list()
        .unwrap();
    let entries_before = engine.entries("Personal", "code", true).unwrap();

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
    assert_eq!(dry.created, 3, "{dry:?}");
    assert_eq!(last_seq(&engine), seq_before);
    assert_eq!(
        engine.entries("Personal", "code", true).unwrap(),
        entries_before
    );
    assert_eq!(
        ObjectStore::open(home.path().join("store"))
            .unwrap()
            .list()
            .unwrap(),
        store_before
    );
    let status = engine.status().unwrap();
    assert!(status.mounts[0].last_scan_ms.is_none());

    let real = scan(&mut engine);
    assert_eq!(dry, real);
    assert!(last_seq(&engine) > seq_before);
    assert!(
        engine
            .status()
            .unwrap()
            .mounts
            .iter()
            .any(|m| m.last_scan_ms == Some(CLOCK_START))
    );
}

#[test]
fn scan_records_success_and_mount_errors() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    fs::write(mount.path().join("a.txt"), b"x").unwrap();
    scan(&mut engine);
    let status = engine.status().unwrap();
    assert_eq!(status.mounts[0].last_scan_ms, Some(CLOCK_START));
    assert!(status.mounts[0].last_error.is_none());

    fs::remove_file(mount.path().join(".relay-mount")).unwrap();
    let err = engine
        .scan("Personal", "code", ScanOptions::default())
        .unwrap_err();
    assert!(matches!(err, EngineError::Fs(_)), "{err}");
    let status = engine.status().unwrap();
    assert_eq!(status.mounts[0].last_scan_ms, Some(CLOCK_START));
    assert!(
        status.mounts[0]
            .last_error
            .as_ref()
            .is_some_and(|e| e.contains("marker")),
        "{status:?}"
    );
}

#[test]
fn own_writes_do_not_change_data_version_other_connection_does() {
    let home = new_home();
    let mount = new_home();
    fs::write(mount.path().join("a.txt"), b"hello").unwrap();
    let mut engine = ready(home.path(), mount.path());
    let before = engine.data_version().unwrap();
    let report = scan(&mut engine);
    assert!(report.created > 0, "{report:?}");
    assert_eq!(
        engine.data_version().unwrap(),
        before,
        "scans write through Engine.db's single rusqlite connection; own commits must not bump data_version"
    );

    let mut other = relay_db::Database::open(&home.path().join("relay.db")).unwrap();
    other
        .transaction(|repo| {
            repo.create_space(
                &relay_core::Space {
                    id: relay_core::SpaceId::new(),
                    name: "Extra".to_owned(),
                },
                1,
            )
        })
        .unwrap();
    assert_ne!(
        engine.data_version().unwrap(),
        before,
        "a commit on a different rusqlite connection must change data_version"
    );
}

#[test]
fn read_only_open_upgrades_an_older_schema() {
    let home = new_home();
    drop(init_engine(home.path()));
    {
        let conn = rusqlite::Connection::open(home.path().join("relay.db")).unwrap();
        conn.execute_batch("DROP TABLE local_settings; PRAGMA user_version = 6;")
            .unwrap();
    }

    let engine = Engine::open_read_only(home.path()).unwrap();
    assert!(!engine.paused().unwrap());
    drop(engine);

    let conn = rusqlite::Connection::open(home.path().join("relay.db")).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 7);
}
