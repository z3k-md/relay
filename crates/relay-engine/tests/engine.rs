use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use relay_engine::{
    Engine, EngineError, EntryContent, LogicalPath, ManualClock, ObjectStore, ScanOptions,
    Sequence, VectorOrdering,
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
        .with_clock(Arc::new(ManualClock::new(CLOCK_START + 60_000)));
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
