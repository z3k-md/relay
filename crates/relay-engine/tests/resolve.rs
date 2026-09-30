use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use relay_core::conflict::conflict_path;
use relay_core::{DeviceId, EntryContent, LogicalPath};
use relay_engine::{
    Engine, EngineConfig, EngineError, ManualClock, Resolution, ScanOptions, resolve_conflict,
    resolve_git_conflicts,
};
use tempfile::TempDir;

const CLOCK_START: i64 = 1_700_000_000_000;

fn ready(home: &Path, mount: &Path) -> Engine {
    let mut engine = Engine::init(home, "testdev")
        .unwrap()
        .with_clock(Arc::new(ManualClock::new(CLOCK_START)))
        .with_config(EngineConfig {
            racy_window: Duration::ZERO,
        });
    engine.create_space("Personal").unwrap();
    engine
        .add_mount("Personal", "code", mount, &[], &[])
        .unwrap();
    engine
}

fn lp(path: &str) -> LogicalPath {
    LogicalPath::new(path).unwrap()
}

fn live_paths(engine: &Engine) -> Vec<String> {
    let mut paths: Vec<String> = engine
        .entries("Personal", "code", false)
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.content, EntryContent::File { .. }))
        .map(|e| e.key.path.to_string())
        .collect();
    paths.sort();
    paths
}

fn seed_file_conflict(home: &Path, mount: &Path) -> LogicalPath {
    let mut engine = ready(home, mount);
    fs::write(mount.join("foo.go"), b"current").unwrap();
    let copy = conflict_path(&lp("foo.go"), &DeviceId::from_bytes([0xab; 32]), 17).unwrap();
    fs::write(mount.join(copy.as_str()), b"losing").unwrap();
    engine
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    drop(engine);
    copy
}

#[test]
fn keep_current_deletes_the_copy() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let copy = seed_file_conflict(home.path(), mount.path());

    let report = resolve_conflict(
        home.path(),
        "Personal",
        "code",
        &copy,
        Resolution::KeepCurrent,
    )
    .unwrap();
    assert!(report.scanned);
    assert_eq!(report.resolution, Resolution::KeepCurrent);
    assert_eq!(fs::read(mount.path().join("foo.go")).unwrap(), b"current");
    assert!(!mount.path().join(copy.as_str()).exists());

    let engine = Engine::open(home.path()).unwrap();
    let paths = live_paths(&engine);
    assert!(paths.contains(&"foo.go".into()));
    assert!(!paths.iter().any(|p| p.contains("relay-conflict")));
}

#[test]
fn use_copy_replaces_original_with_on_disk_bytes() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let copy = seed_file_conflict(home.path(), mount.path());
    fs::write(mount.path().join(copy.as_str()), b"edited-on-copy").unwrap();

    let report =
        resolve_conflict(home.path(), "Personal", "code", &copy, Resolution::UseCopy).unwrap();
    assert!(report.scanned);
    assert_eq!(
        fs::read(mount.path().join("foo.go")).unwrap(),
        b"edited-on-copy"
    );
    assert!(!mount.path().join(copy.as_str()).exists());

    let engine = Engine::open(home.path()).unwrap();
    let foo = engine
        .entries("Personal", "code", false)
        .unwrap()
        .into_iter()
        .find(|e| e.key.path.as_str() == "foo.go")
        .unwrap();
    match foo.content {
        EntryContent::File { size, .. } => assert_eq!(size, b"edited-on-copy".len() as u64),
        other => panic!("{other:?}"),
    }
    assert!(
        !live_paths(&engine)
            .iter()
            .any(|p| p.contains("relay-conflict"))
    );
}

#[test]
fn resolve_git_conflicts_keeps_refs_unless_requested() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let device = DeviceId::from_bytes([0xcd; 32]);
    let index_copy = conflict_path(&lp("repo/.git/index"), &device, 3).unwrap();
    let head_copy = conflict_path(&lp("repo/.git/HEAD"), &device, 4).unwrap();
    let ref_copy = conflict_path(&lp("repo/.git/refs/heads/main"), &device, 5).unwrap();

    {
        let mut engine = ready(home.path(), mount.path());
        fs::create_dir_all(mount.path().join("repo/.git/refs/heads")).unwrap();
        fs::write(
            mount.path().join("repo/.git/HEAD"),
            b"ref: refs/heads/main\n",
        )
        .unwrap();
        fs::write(mount.path().join("repo/.git/index"), b"DIRC").unwrap();
        fs::write(mount.path().join("repo/.git/refs/heads/main"), b"abc\n").unwrap();
        fs::write(mount.path().join(index_copy.as_str()), b"DIRC-lose").unwrap();
        fs::write(mount.path().join(head_copy.as_str()), b"other-head").unwrap();
        fs::write(mount.path().join(ref_copy.as_str()), b"def\n").unwrap();
        engine
            .scan("Personal", "code", ScanOptions::default())
            .unwrap();
    }

    let report =
        resolve_git_conflicts(home.path(), "Personal", "code", &lp("repo/.git"), false).unwrap();
    assert!(report.scanned);
    assert!(!mount.path().join(index_copy.as_str()).exists());
    assert!(!mount.path().join(head_copy.as_str()).exists());
    assert!(mount.path().join(ref_copy.as_str()).exists());
    assert!(report.kept.iter().any(|p| p == &ref_copy));

    let engine = Engine::open(home.path()).unwrap();
    let paths = live_paths(&engine);
    assert!(paths.iter().any(|p| p == ref_copy.as_str()));
    assert!(!paths.iter().any(|p| p == index_copy.as_str()));

    drop(engine);
    let report =
        resolve_git_conflicts(home.path(), "Personal", "code", &lp("repo/.git"), true).unwrap();
    assert!(report.scanned);
    assert!(!mount.path().join(ref_copy.as_str()).exists());
}

#[test]
fn resolve_refuses_a_path_that_is_not_a_conflict_copy() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    {
        let mut engine = ready(home.path(), mount.path());
        fs::write(mount.path().join("foo.go"), b"ok").unwrap();
        engine
            .scan("Personal", "code", ScanOptions::default())
            .unwrap();
    }
    let err = resolve_conflict(
        home.path(),
        "Personal",
        "code",
        &lp("foo.go"),
        Resolution::KeepCurrent,
    )
    .unwrap_err();
    assert!(matches!(err, EngineError::NotAConflictCopy(_)), "{err:?}");
}

#[test]
fn resolve_scans_when_no_loop_is_running() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let copy = seed_file_conflict(home.path(), mount.path());
    let report = resolve_conflict(
        home.path(),
        "Personal",
        "code",
        &copy,
        Resolution::KeepCurrent,
    )
    .unwrap();
    assert!(report.scanned, "{report:?}");
    let engine = Engine::open(home.path()).unwrap();
    assert!(
        !engine
            .conflicts(None)
            .unwrap()
            .iter()
            .any(|e| e.key.path == copy)
    );
}
