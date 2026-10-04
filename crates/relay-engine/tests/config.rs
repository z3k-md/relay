//! `ConfigChange` applied directly (no host running).

use std::fs;
use std::path::Path;

use relay_engine::{ConfigApplied, ConfigChange, Engine, EngineError, ScanOptions};
use tempfile::TempDir;

const MARKER: &str = ".relay-mount";

fn add_mount(space: &str, mount: &str, path: &Path) -> ConfigChange {
    ConfigChange::AddMount {
        space: space.into(),
        mount: mount.into(),
        path: path.to_path_buf(),
        includes: Vec::new(),
        excludes: Vec::new(),
    }
}

fn remove_mount(space: &str, mount: &str) -> ConfigChange {
    ConfigChange::RemoveMount {
        space: space.into(),
        mount: mount.into(),
    }
}

fn ready(home: &Path, folder: &Path) -> Engine {
    let mut engine = Engine::init(home, "dev").unwrap();
    let created = engine
        .apply_config(&ConfigChange::CreateSpace {
            space: "Personal".into(),
        })
        .unwrap();
    assert!(matches!(created, ConfigApplied::Space { .. }));
    engine
        .apply_config(&add_mount("Personal", "code", folder))
        .unwrap();
    engine
}

fn scan(engine: &mut Engine) -> relay_engine::ScanReport {
    engine
        .scan("Personal", "code", ScanOptions::default())
        .unwrap()
}

#[test]
fn remove_mount_keeps_files_and_reattach_is_not_a_mass_delete() {
    let home = TempDir::new().unwrap();
    let folder = TempDir::new().unwrap();
    let mut engine = ready(home.path(), folder.path());
    fs::write(folder.path().join("a.txt"), b"a").unwrap();
    assert_eq!(scan(&mut engine).created, 1);

    engine
        .apply_config(&remove_mount("Personal", "code"))
        .unwrap();

    assert!(folder.path().join("a.txt").exists(), "files stay on disk");
    assert!(!folder.path().join(MARKER).exists(), "marker is removed");
    let (_, config) = engine.mounts(Some("Personal")).unwrap().remove(0);
    assert_eq!(config.local_path, None, "mount is unattached");
    assert!(
        engine.entries("Personal", "code", true).unwrap().is_empty(),
        "an unattached mount stores no entries"
    );

    // Kept index rows would read as "every file was deleted" here.
    let elsewhere = TempDir::new().unwrap();
    engine
        .apply_config(&add_mount("Personal", "code", elsewhere.path()))
        .unwrap();
    let report = scan(&mut engine);
    assert_eq!((report.created, report.deleted), (0, 0));
}

#[test]
fn leftover_marker_of_a_removed_mount_is_adopted() {
    let home = TempDir::new().unwrap();
    let folder = TempDir::new().unwrap();
    let mut engine = ready(home.path(), folder.path());
    let marker = fs::read(folder.path().join(MARKER)).unwrap();

    engine
        .apply_config(&remove_mount("Personal", "code"))
        .unwrap();
    // As if deleting the marker had failed after the detach committed.
    fs::write(folder.path().join(MARKER), marker).unwrap();

    engine
        .apply_config(&add_mount("Personal", "code", folder.path()))
        .expect("a marker this device wrote for a detached mount is adoptable");
}

#[test]
fn remove_mount_requires_an_attached_mount() {
    let home = TempDir::new().unwrap();
    let folder = TempDir::new().unwrap();
    let mut engine = ready(home.path(), folder.path());
    engine
        .apply_config(&remove_mount("Personal", "code"))
        .unwrap();
    let err = engine
        .apply_config(&remove_mount("Personal", "code"))
        .unwrap_err();
    assert!(matches!(err, EngineError::MountNotLocal), "{err}");
}

#[test]
fn delete_space_requires_detached_mounts_and_leaves_files() {
    let home = TempDir::new().unwrap();
    let folder = TempDir::new().unwrap();
    let mut engine = ready(home.path(), folder.path());
    fs::write(folder.path().join("a.txt"), b"a").unwrap();
    scan(&mut engine);
    engine
        .apply_config(&ConfigChange::MaterializeAdd {
            space: "Personal".into(),
            name: "media".into(),
            mode: "demand".into(),
            selectors: vec!["code/media/**".into()],
        })
        .unwrap();

    let delete = ConfigChange::DeleteSpace {
        space: "Personal".into(),
    };
    let err = engine.apply_config(&delete).unwrap_err();
    assert!(
        matches!(err, EngineError::SpaceHasAttachedMounts { ref mounts, .. } if mounts == &["code"]),
        "{err}"
    );

    engine
        .apply_config(&remove_mount("Personal", "code"))
        .unwrap();
    engine.apply_config(&delete).unwrap();
    assert!(engine.spaces().unwrap().is_empty());
    assert!(engine.materialization_rules(None).unwrap().is_empty());
    assert!(folder.path().join("a.txt").exists());

    // The name is free again.
    engine
        .apply_config(&ConfigChange::CreateSpace {
            space: "Personal".into(),
        })
        .unwrap();
}

#[test]
fn manage_grant_is_explicit_and_revoke_clears_it() {
    let home = TempDir::new().unwrap();
    let mut engine = Engine::init(home.path(), "dev").unwrap();
    let id = relay_engine::DeviceIdentity::generate(TempDir::new().unwrap().path())
        .unwrap()
        .device_id();
    engine
        .apply_config(&ConfigChange::AddPeer {
            peer: "laptop".into(),
            id,
            addresses: vec!["127.0.0.1:1".into()],
        })
        .unwrap();
    let grant = |engine: &Engine| engine.peers().unwrap()[0].may_manage;
    assert!(!grant(&engine), "adding a peer grants nothing");

    let allow = ConfigChange::SetPeerManage {
        peer: "laptop".into(),
        allowed: true,
    };
    engine.apply_config(&allow).unwrap();
    assert!(grant(&engine));

    engine
        .apply_config(&ConfigChange::RevokePeer {
            peer: "laptop".into(),
        })
        .unwrap();
    assert!(!grant(&engine), "revoke takes the grant back");
    let err = engine.apply_config(&allow).unwrap_err();
    assert!(matches!(err, EngineError::PeerRevoked(_)), "{err}");
}
