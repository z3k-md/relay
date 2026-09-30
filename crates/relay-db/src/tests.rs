use std::path::PathBuf;

use proptest::prelude::*;
use relay_core::{
    Device, DeviceId, EntryContent, EntryKey, EntryRecord, LogicalPath, Mount, MountId, ObjectId,
    Sequence, Space, SpaceId, StatHint, VersionVector,
};

use rusqlite::params;

use crate::{Database, DbError};

fn device(n: u8, name: &str) -> Device {
    Device {
        id: DeviceId::from_bytes([n; 32]),
        name: name.to_owned(),
    }
}

fn space(name: &str) -> Space {
    Space {
        id: SpaceId::new(),
        name: name.to_owned(),
    }
}

fn mount(space: SpaceId, name: &str) -> Mount {
    Mount {
        id: MountId::new(),
        space,
        name: name.to_owned(),
    }
}

fn path(s: &str) -> LogicalPath {
    LogicalPath::new(s).expect("valid logical path")
}

fn file(data: &[u8], executable: bool) -> EntryContent {
    EntryContent::File {
        object: ObjectId::of(data),
        size: u64::try_from(data.len()).expect("file size fits u64"),
        executable,
    }
}

fn key(space: SpaceId, mount: MountId, p: &str) -> EntryKey {
    EntryKey {
        space,
        mount,
        path: path(p),
    }
}

fn vector(pairs: &[(u8, u64)]) -> VersionVector {
    pairs
        .iter()
        .map(|(d, c)| (DeviceId::from_bytes([*d; 32]), *c))
        .collect()
}

struct Harness {
    db: Database,
    local: Device,
    space: Space,
    mount: Mount,
}

impl Harness {
    fn new() -> Self {
        let db = Database::open_in_memory().expect("open in-memory db");
        let local = device(1, "desktop");
        let space = space("Personal");
        let mount = mount(space.id, "code");
        db.repo()
            .init_local_device(&local, 1_000)
            .expect("init local device");
        db.repo().create_space(&space, 1_000).expect("create space");
        db.repo().create_mount(&mount, 1_000).expect("create mount");
        Self {
            db,
            local,
            space,
            mount,
        }
    }

    fn record(
        &self,
        p: &str,
        content: EntryContent,
        sequence: u64,
        vv: VersionVector,
        stat: Option<StatHint>,
        parent_object: Option<ObjectId>,
    ) -> EntryRecord {
        EntryRecord {
            key: key(self.space.id, self.mount.id, p),
            content,
            vector: vv,
            parent_object,
            sequence: Sequence(sequence),
            modified_by: self.local.id,
            modified_at_unix_ms: 2_000,
            stat,
        }
    }
}

#[test]
fn data_version_changes_only_when_another_connection_commits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.db");
    let mut writer = Database::open(&path).unwrap();
    writer
        .transaction(|repo| repo.init_local_device(&device(1, "desktop"), 1_000))
        .unwrap();
    let before_own = writer.data_version().unwrap();
    writer
        .transaction(|repo| repo.create_space(&space("Personal"), 1_000))
        .unwrap();
    assert_eq!(
        writer.data_version().unwrap(),
        before_own,
        "own commits must not change PRAGMA data_version on the writing connection"
    );

    let mut other = Database::open(&path).unwrap();
    let before_external = writer.data_version().unwrap();
    other
        .transaction(|repo| repo.create_space(&space("Extra"), 2_000))
        .unwrap();
    assert_ne!(
        writer.data_version().unwrap(),
        before_external,
        "a commit on another connection must change data_version"
    );
    let other_before = other.data_version().unwrap();
    other
        .transaction(|repo| repo.create_space(&space("Third"), 3_000))
        .unwrap();
    assert_eq!(
        other.data_version().unwrap(),
        other_before,
        "the committing connection must not see its own write as a data_version change"
    );
}

#[test]
fn migrations_are_idempotent_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("relay.sqlite");
    {
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 7);
    }
    {
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 7);
        db.repo().init_local_device(&device(9, "again"), 1).unwrap();
        assert_eq!(db.schema_version().unwrap(), 7);
    }
}

#[test]
fn schema_too_new_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.sqlite");
    Database::open(&path).unwrap();
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99u32).unwrap();
    }
    let err = Database::open(&path).unwrap_err();
    assert!(matches!(
        err,
        DbError::SchemaTooNew {
            found: 99,
            supported: 7
        }
    ));
}

fn write_v1_db(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(include_str!("../migrations/0001_init.sql"))
        .unwrap();
    conn.pragma_update(None, "user_version", 1u32).unwrap();
    let device = [9u8; 32];
    let space = [1u8; 16];
    let mount = [2u8; 16];
    conn.execute(
        "INSERT INTO devices (device_id, name, status, created_at_ms)
         VALUES (?1, 'legacy', 'active', 1)",
        params![device.as_slice()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO local_device (singleton, device_ref, next_sequence) VALUES (1, 1, 7)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO spaces (id, name, created_at_ms) VALUES (?1, 'Legacy', 1)",
        params![space.as_slice()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO mounts (id, space_id, name, created_at_ms) VALUES (?1, ?2, 'docs', 1)",
        params![mount.as_slice(), space.as_slice()],
    )
    .unwrap();
}

#[test]
fn v1_database_upgrades_to_current_without_data_loss() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.sqlite");
    write_v1_db(&path);

    let db = Database::open(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 7);
    let space = db.repo().space_by_name("Legacy").unwrap().unwrap();
    assert_eq!(space.name, "Legacy");
    let mount = db.repo().mount_by_name(space.id, "docs").unwrap().unwrap();
    assert_eq!(mount.name, "docs");
    let local = db.repo().local_device().unwrap().unwrap();
    assert_eq!(local.device.name, "legacy");
    assert_eq!(local.next_sequence, Sequence(7));
    assert!(db.repo().mount_state(mount.id).unwrap().is_none());
}

#[test]
fn upgrade_collapses_duplicate_history_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.sqlite");
    write_v1_db(&path);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO entries (id, mount_id, path, kind, deleted, sequence, modified_by, modified_at_ms)
             VALUES (1, ?1, 'a.txt', 'directory', 0, 5, 1, 1)",
            params![[2u8; 16].as_slice()],
        )
        .unwrap();
        for _ in 0..2 {
            conn.execute(
                "INSERT INTO history (entry_id, sequence, kind, deleted, executable, vector_json, modified_by, modified_at_ms)
                 VALUES (1, 5, 'directory', 0, 0, '{}', 1, 1)",
                [],
            )
            .unwrap();
        }
    }

    let db = Database::open(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 7);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1);
}

#[test]
fn open_read_only_does_not_migrate_and_rejects_version_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.sqlite");
    write_v1_db(&path);

    let err = Database::open_read_only(&path).unwrap_err();
    assert!(
        matches!(
            err,
            DbError::SchemaTooOld {
                found: 1,
                supported: 7
            }
        ),
        "{err}"
    );
    let version: i64 = {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(version, 1);

    Database::open(&path).unwrap();
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99u32).unwrap();
    }
    let err = Database::open_read_only(&path).unwrap_err();
    assert!(
        matches!(
            err,
            DbError::SchemaTooNew {
                found: 99,
                supported: 7
            }
        ),
        "{err}"
    );
}

#[test]
fn open_read_only_reads_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.sqlite");
    let space_name;
    {
        let db = Database::open(&path).unwrap();
        db.repo()
            .init_local_device(&device(1, "reader"), 1)
            .unwrap();
        let space = space("Personal");
        space_name = space.name.clone();
        db.repo().create_space(&space, 1).unwrap();
    }
    let db = Database::open_read_only(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 7);
    let names: Vec<_> = db
        .repo()
        .list_spaces()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec![space_name]);
}

fn write_v4_db(path: &std::path::Path) {
    write_v1_db(path);
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(include_str!("../migrations/0002_mount_state.sql"))
        .unwrap();
    conn.execute_batch(include_str!(
        "../migrations/0003_stat_ctime_and_history_unique.sql"
    ))
    .unwrap();
    conn.execute_batch(include_str!("../migrations/0004_peers_and_sync.sql"))
        .unwrap();
    conn.pragma_update(None, "user_version", 4u32).unwrap();
}

#[test]
fn v4_database_upgrades_to_delete_holds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.sqlite");
    write_v4_db(&path);

    let version: i64 = {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(version, 4);

    let db = Database::open(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 7);
    let space = db.repo().space_by_name("Legacy").unwrap().unwrap();
    assert_eq!(space.name, "Legacy");
    let mount = db.repo().mount_by_name(space.id, "docs").unwrap().unwrap();
    assert_eq!(mount.name, "docs");
    assert!(db.repo().list_delete_holds().unwrap().is_empty());
    assert!(db.repo().local_setting("paused").unwrap().is_none());
}

#[test]
fn fresh_database_has_delete_hold_tables() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(db.schema_version().unwrap(), 7);
    db.repo().init_local_device(&device(1, "dev"), 1).unwrap();
    let space = space("Personal");
    db.repo().create_space(&space, 1).unwrap();
    let mount = mount(space.id, "code");
    db.repo().create_mount(&mount, 1).unwrap();
    let peer = device(9, "laptop");
    db.repo()
        .add_peer(&peer, &["127.0.0.1:1".into()], 2)
        .unwrap();

    db.repo()
        .upsert_delete_hold(peer.id, space.id, mount.id, 30, 40, 1_000)
        .unwrap();
    db.repo()
        .replace_delete_hold_paths(peer.id, space.id, mount.id, &[path("a.txt"), path("b.txt")])
        .unwrap();

    let holds = db.repo().list_delete_holds().unwrap();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].deletions, 30);
    assert_eq!(holds[0].live, 40);
    assert!(holds[0].decision.is_none());
    db.repo()
        .insert_delete_hold_applied_paths(peer.id, space.id, mount.id, &[path("gone.txt")])
        .unwrap();

    let paths = db
        .repo()
        .list_delete_hold_paths(peer.id, space.id, mount.id)
        .unwrap();
    assert_eq!(
        paths.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        vec!["a.txt", "b.txt"]
    );
    let applied = db
        .repo()
        .list_delete_hold_applied_paths(peer.id, space.id, mount.id)
        .unwrap();
    assert_eq!(
        applied.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        vec!["gone.txt"]
    );

    let n = db
        .repo()
        .decide_delete_holds(
            space.id,
            Some(mount.id),
            Some(peer.id),
            crate::DeleteHoldDecision::Restore,
            2_000,
        )
        .unwrap();
    assert_eq!(n, 1);
    let hold = db
        .repo()
        .delete_hold(peer.id, space.id, mount.id)
        .unwrap()
        .unwrap();
    assert_eq!(hold.decision, Some(crate::DeleteHoldDecision::Restore));
    assert_eq!(hold.decided_at_ms, Some(2_000));

    db.repo()
        .clear_delete_hold(peer.id, space.id, mount.id)
        .unwrap();
    assert!(db.repo().list_delete_holds().unwrap().is_empty());
    assert!(
        db.repo()
            .list_delete_hold_paths(peer.id, space.id, mount.id)
            .unwrap()
            .is_empty()
    );
    assert!(
        db.repo()
            .list_delete_hold_applied_paths(peer.id, space.id, mount.id)
            .unwrap()
            .is_empty()
    );
}

fn write_v5_db(path: &std::path::Path) {
    write_v4_db(path);
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(include_str!("../migrations/0005_delete_holds.sql"))
        .unwrap();
    let peer = [8u8; 32];
    conn.execute(
        "INSERT INTO devices (device_id, name, status, created_at_ms)
         VALUES (?1, 'laptop', 'active', 2)",
        params![peer.as_slice()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO peers (device_ref, name, addresses, added_at_ms)
         VALUES (2, 'laptop', '[]', 2)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO delete_holds (device_ref, space_id, mount_id, deletions, live, held_at_ms)
         VALUES (2, ?1, ?2, 30, 40, 1000)",
        params![[1u8; 16].as_slice(), [2u8; 16].as_slice()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO delete_hold_paths (device_ref, space_id, mount_id, path)
         VALUES (2, ?1, ?2, 'held.txt')",
        params![[1u8; 16].as_slice(), [2u8; 16].as_slice()],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 5u32).unwrap();
}

#[test]
fn v5_database_upgrades_to_applied_hold_paths() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("relay.sqlite");
    write_v5_db(&db_path);

    let version: i64 = {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(version, 5);

    let db = Database::open(&db_path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 7);
    let space = db.repo().space_by_name("Legacy").unwrap().unwrap();
    let mount = db.repo().mount_by_name(space.id, "docs").unwrap().unwrap();
    let peer = db.repo().peer_by_name("laptop").unwrap().unwrap();

    assert_eq!(
        db.repo()
            .list_delete_hold_paths(peer.device.id, space.id, mount.id)
            .unwrap()
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>(),
        vec!["held.txt"]
    );
    assert!(
        db.repo()
            .list_delete_hold_applied_paths(peer.device.id, space.id, mount.id)
            .unwrap()
            .is_empty()
    );

    db.repo()
        .insert_delete_hold_applied_paths(peer.device.id, space.id, mount.id, &[path("gone.txt")])
        .unwrap();
    assert_eq!(
        db.repo()
            .list_delete_hold_applied_paths(peer.device.id, space.id, mount.id)
            .unwrap()
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>(),
        vec!["gone.txt"]
    );
    assert_eq!(
        db.repo()
            .list_delete_hold_paths(peer.device.id, space.id, mount.id)
            .unwrap()
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>(),
        vec!["held.txt"]
    );
}

#[test]
fn mount_state_success_and_error() {
    let h = Harness::new();
    let repo = h.db.repo();
    assert!(repo.mount_state(h.mount.id).unwrap().is_none());

    repo.record_scan_success(h.mount.id, true, 1_000).unwrap();
    let state = repo.mount_state(h.mount.id).unwrap().unwrap();
    assert_eq!(state.last_scan_ms, Some(1_000));
    assert_eq!(state.last_full_scan_ms, Some(1_000));
    assert!(state.last_error.is_none());
    assert!(state.last_error_ms.is_none());

    repo.record_scan_error(h.mount.id, "marker missing", 2_000)
        .unwrap();
    let state = repo.mount_state(h.mount.id).unwrap().unwrap();
    assert_eq!(state.last_scan_ms, Some(1_000));
    assert_eq!(state.last_full_scan_ms, Some(1_000));
    assert_eq!(state.last_error.as_deref(), Some("marker missing"));
    assert_eq!(state.last_error_ms, Some(2_000));

    repo.record_scan_success(h.mount.id, false, 3_000).unwrap();
    let state = repo.mount_state(h.mount.id).unwrap().unwrap();
    assert_eq!(state.last_scan_ms, Some(3_000));
    assert_eq!(state.last_full_scan_ms, Some(1_000));
    assert!(state.last_error.is_none());
    assert!(state.last_error_ms.is_none());

    let other = MountId::new();
    assert!(matches!(
        repo.record_scan_success(other, true, 4_000),
        Err(DbError::NotFound)
    ));
}

#[test]
fn local_device_init_and_next_sequence_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.sqlite");
    let local = device(1, "laptop");
    let first;
    let second;
    {
        let db = Database::open(&path).unwrap();
        db.repo().init_local_device(&local, 10).unwrap();
        assert!(matches!(
            db.repo().init_local_device(&local, 11),
            Err(DbError::AlreadyInitialized)
        ));
        let loaded = db.repo().local_device().unwrap().unwrap();
        assert_eq!(loaded.device, local);
        assert_eq!(loaded.next_sequence, Sequence(1));
        first = db.repo().next_sequence().unwrap();
        second = db.repo().next_sequence().unwrap();
        assert_eq!(first, Sequence(1));
        assert_eq!(second, Sequence(2));
        assert_eq!(
            db.repo().local_device().unwrap().unwrap().next_sequence,
            Sequence(3)
        );
    }
    {
        let db = Database::open(&path).unwrap();
        let loaded = db.repo().local_device().unwrap().unwrap();
        assert_eq!(loaded.device, local);
        assert_eq!(loaded.next_sequence, Sequence(3));
        let third = db.repo().next_sequence().unwrap();
        assert_eq!(third, Sequence(3));
        assert!(third > second && second > first);
        db.repo().upsert_device(&device(2, "phone"), 20).unwrap();
        let names: Vec<_> = db
            .repo()
            .list_devices()
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["laptop".to_owned(), "phone".to_owned()]);
    }
}

#[test]
fn spaces_and_mounts_round_trip() {
    let h = Harness::new();
    let repo = h.db.repo();

    assert_eq!(repo.space(h.space.id).unwrap().as_ref(), Some(&h.space));
    assert_eq!(
        repo.space_by_name("Personal").unwrap().as_ref(),
        Some(&h.space)
    );
    assert_eq!(repo.list_spaces().unwrap(), vec![h.space.clone()]);

    let dup = Space {
        id: SpaceId::new(),
        name: "Personal".into(),
    };
    assert!(matches!(
        repo.create_space(&dup, 3),
        Err(DbError::DuplicateName(name)) if name == "Personal"
    ));

    let other_space = space("Work");
    repo.create_space(&other_space, 4).unwrap();
    let same_name_other_space = mount(other_space.id, "code");
    repo.create_mount(&same_name_other_space, 4).unwrap();

    let dup_mount = Mount {
        id: MountId::new(),
        space: h.space.id,
        name: "code".into(),
    };
    assert!(matches!(
        repo.create_mount(&dup_mount, 5),
        Err(DbError::DuplicateName(name)) if name == "code"
    ));

    assert_eq!(
        repo.mount_by_name(h.space.id, "code").unwrap().as_ref(),
        Some(&h.mount)
    );

    repo.set_mount_rules(
        h.mount.id,
        &["src/**".into(), "docs/**".into()],
        &["target/**".into()],
    )
    .unwrap();
    repo.set_local_mount_path(h.mount.id, &PathBuf::from("/tmp/relay-code"))
        .unwrap();

    let cfg = repo.mount_config(h.mount.id).unwrap().unwrap();
    assert_eq!(cfg.mount, h.mount);
    assert_eq!(cfg.local_path, Some(PathBuf::from("/tmp/relay-code")));
    assert_eq!(cfg.includes, vec!["src/**", "docs/**"]);
    assert_eq!(cfg.excludes, vec!["target/**"]);

    repo.set_mount_rules(h.mount.id, &["*.lua".into()], &[])
        .unwrap();
    let cfg = repo.mount_config(h.mount.id).unwrap().unwrap();
    assert_eq!(cfg.includes, vec!["*.lua"]);
    assert!(cfg.excludes.is_empty());

    let listed = repo.list_mounts(Some(h.space.id)).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].mount, h.mount);
    assert_eq!(listed[0].includes, vec!["*.lua"]);

    let all = repo.list_mounts(None).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn put_entry_round_trips_all_content_kinds_and_history() {
    let h = Harness::new();
    let repo = h.db.repo();
    let vv = vector(&[(1, 10), (2, 20), (3, 30)]);
    let parent = ObjectId::of(b"parent");
    let stat = StatHint {
        size: 42,
        mtime_ns: 1_000_000,
        file_id: Some(99),
        ctime_ns: Some(2_000_000),
    };

    let file_rec = h.record(
        "src/a.rs",
        file(b"hello", true),
        1,
        vv.clone(),
        Some(stat),
        Some(parent),
    );
    repo.put_entry(&file_rec).unwrap();
    assert_eq!(repo.entry(&file_rec.key).unwrap().as_ref(), Some(&file_rec));

    let dir_rec = h.record("src", EntryContent::Directory, 2, vv.clone(), None, None);
    repo.put_entry(&dir_rec).unwrap();
    assert_eq!(repo.entry(&dir_rec.key).unwrap().as_ref(), Some(&dir_rec));

    let link_rec = h.record(
        "src/link",
        EntryContent::Symlink {
            target: "../other".into(),
        },
        3,
        vv.clone(),
        None,
        None,
    );
    repo.put_entry(&link_rec).unwrap();
    assert_eq!(repo.entry(&link_rec.key).unwrap().as_ref(), Some(&link_rec));

    let deleted_rec = h.record("gone.txt", EntryContent::Deleted, 4, vv.clone(), None, None);
    repo.put_entry(&deleted_rec).unwrap();
    assert_eq!(
        repo.entry(&deleted_rec.key).unwrap().as_ref(),
        Some(&deleted_rec)
    );

    let placeholders: Vec<_> = repo
        .list_devices()
        .unwrap()
        .into_iter()
        .filter(|d| {
            d.name == DeviceId::from_bytes([2; 32]).short()
                || d.name == DeviceId::from_bytes([3; 32]).short()
        })
        .map(|d| d.id)
        .collect();
    assert!(placeholders.contains(&DeviceId::from_bytes([2; 32])));
    assert!(placeholders.contains(&DeviceId::from_bytes([3; 32])));

    let updated = h.record(
        "src/a.rs",
        file(b"hello2", false),
        5,
        vector(&[(1, 11), (2, 20), (3, 30)]),
        Some(StatHint {
            size: 6,
            mtime_ns: 2,
            file_id: Some(7),
            ctime_ns: Some(8),
        }),
        file_rec.content.object(),
    );
    repo.put_entry(&updated).unwrap();

    let mount_entries = repo.entries_for_mount(h.mount.id).unwrap();
    assert_eq!(mount_entries.len(), 4);
    let paths: Vec<_> = mount_entries
        .iter()
        .map(|e| e.key.path.as_str().to_owned())
        .collect();
    assert_eq!(paths, vec!["gone.txt", "src", "src/a.rs", "src/link"]);
    assert_eq!(
        mount_entries
            .iter()
            .find(|e| e.key.path.as_str() == "src/a.rs")
            .unwrap(),
        &updated
    );

    let hist = repo.history(&file_rec.key).unwrap();
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0].sequence, Sequence(1));
    assert_eq!(hist[0].content, file_rec.content);
    assert_eq!(hist[0].vector, file_rec.vector);
    assert_eq!(hist[0].parent_object, file_rec.parent_object);
    assert_eq!(hist[0].modified_by, file_rec.modified_by);
    assert_eq!(hist[0].modified_at_unix_ms, file_rec.modified_at_unix_ms);
    assert_eq!(hist[1].sequence, Sequence(5));
    assert_eq!(hist[1].content, updated.content);

    let since = repo.changes_since(Sequence(2), 2).unwrap();
    assert_eq!(
        since.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        vec![Sequence(3), Sequence(4)]
    );
    let rest = repo.changes_since(Sequence(4), 10).unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].sequence, Sequence(5));
}

#[test]
fn put_entry_identical_record_leaves_one_history_row() {
    let h = Harness::new();
    let rec = h.record(
        "same.txt",
        file(b"once", false),
        1,
        vector(&[(1, 1)]),
        Some(StatHint {
            size: 4,
            mtime_ns: 10,
            file_id: Some(1),
            ctime_ns: Some(11),
        }),
        None,
    );
    h.db.repo().put_entry(&rec).unwrap();
    h.db.repo().put_entry(&rec).unwrap();
    assert_eq!(h.db.repo().entry(&rec.key).unwrap().as_ref(), Some(&rec));
    assert_eq!(h.db.repo().history(&rec.key).unwrap().len(), 1);
}

#[test]
fn put_entry_rejects_space_mismatch() {
    let h = Harness::new();
    let mut rec = h.record("x.txt", file(b"x", false), 1, vector(&[(1, 1)]), None, None);
    rec.key.space = SpaceId::new();
    assert!(matches!(
        h.db.repo().put_entry(&rec),
        Err(DbError::SpaceMismatch)
    ));
}

#[test]
fn update_stat_changes_only_stat() {
    let h = Harness::new();
    let rec = h.record(
        "a.bin",
        file(b"abc", false),
        1,
        vector(&[(1, 1)]),
        Some(StatHint {
            size: 3,
            mtime_ns: 10,
            file_id: Some(1),
            ctime_ns: Some(11),
        }),
        None,
    );
    h.db.repo().put_entry(&rec).unwrap();
    let new_stat = StatHint {
        size: 3,
        mtime_ns: 99,
        file_id: Some(2),
        ctime_ns: Some(100),
    };
    h.db.repo().update_stat(&rec.key, Some(new_stat)).unwrap();
    let got = h.db.repo().entry(&rec.key).unwrap().unwrap();
    assert_eq!(got.stat, Some(new_stat));
    assert_eq!(got.content, rec.content);
    assert_eq!(got.vector, rec.vector);
    assert_eq!(got.sequence, rec.sequence);
    assert_eq!(got.parent_object, rec.parent_object);
    assert_eq!(h.db.repo().history(&rec.key).unwrap().len(), 1);

    h.db.repo().update_stat(&rec.key, None).unwrap();
    assert_eq!(h.db.repo().entry(&rec.key).unwrap().unwrap().stat, None);
}

#[test]
fn live_objects_include_history_only_references() {
    let h = Harness::new();
    let repo = h.db.repo();
    let first = ObjectId::of(b"one");
    let second = ObjectId::of(b"two");
    let orphan = ObjectId::of(b"orphan");
    repo.record_object(first, 3, 1).unwrap();
    repo.record_object(first, 99, 2).unwrap();
    repo.record_object(second, 3, 3).unwrap();
    repo.record_object(orphan, 1, 4).unwrap();
    assert_eq!(repo.object_count().unwrap(), 3);

    let v1 = h.record(
        "doc.txt",
        EntryContent::File {
            object: first,
            size: 3,
            executable: false,
        },
        1,
        vector(&[(1, 1)]),
        None,
        None,
    );
    repo.put_entry(&v1).unwrap();
    let v2 = h.record(
        "doc.txt",
        EntryContent::File {
            object: second,
            size: 3,
            executable: false,
        },
        2,
        vector(&[(1, 2)]),
        None,
        Some(first),
    );
    repo.put_entry(&v2).unwrap();
    let tomb = h.record(
        "doc.txt",
        EntryContent::Deleted,
        3,
        vector(&[(1, 3)]),
        None,
        Some(second),
    );
    repo.put_entry(&tomb).unwrap();

    let live = repo.live_objects().unwrap();
    assert!(
        live.contains(&first),
        "history still references first object"
    );
    assert!(live.contains(&second));
    assert!(!live.contains(&orphan));
}

#[test]
fn transaction_rolls_back_on_error() {
    let mut h = Harness::new();
    let extra = space("Scratch");
    let result: Result<(), DbError> = h.db.transaction(|repo| {
        repo.create_space(&extra, 9)?;
        assert!(repo.space(extra.id).unwrap().is_some());
        Err(DbError::Corrupt("forced rollback".into()))
    });
    assert!(result.is_err());
    assert!(h.db.repo().space(extra.id).unwrap().is_none());
    assert!(
        h.db.repo()
            .list_spaces()
            .unwrap()
            .iter()
            .all(|s| s.name != "Scratch")
    );

    h.db.transaction(|repo| {
        repo.create_space(&extra, 9)?;
        Ok::<(), DbError>(())
    })
    .unwrap();
    assert_eq!(h.db.repo().space(extra.id).unwrap().as_ref(), Some(&extra));
}

#[test]
fn entries_under_is_a_range_not_like() {
    let h = Harness::new();
    let vv = vector(&[(1, 1)]);
    let paths = [
        "a/b", "a-b", "ab", "a/%weird", "a/_under", "a/plain", "other",
    ];
    for (i, p) in paths.iter().enumerate() {
        let rec = h.record(p, file(b"x", false), (i + 1) as u64, vv.clone(), None, None);
        h.db.repo().put_entry(&rec).unwrap();
    }
    let dir = h.record("a", EntryContent::Directory, 8, vv, None, None);
    h.db.repo().put_entry(&dir).unwrap();

    let under_a = h.db.repo().entries_under(h.mount.id, &path("a")).unwrap();
    let got: Vec<_> = under_a.iter().map(|e| e.key.path.as_str()).collect();
    assert_eq!(got, vec!["a", "a/%weird", "a/_under", "a/b", "a/plain"]);

    let under_ab = h.db.repo().entries_under(h.mount.id, &path("a/b")).unwrap();
    assert_eq!(under_ab.len(), 1);
    assert_eq!(under_ab[0].key.path.as_str(), "a/b");

    let sibling = h.db.repo().entries_under(h.mount.id, &path("a-b")).unwrap();
    assert_eq!(sibling.len(), 1);
    assert_eq!(sibling[0].key.path.as_str(), "a-b");
}

#[test]
fn count_live_skips_tombstones() {
    let h = Harness::new();
    let vv = vector(&[(1, 1)]);
    h.db.repo()
        .put_entry(&h.record("live.txt", file(b"x", false), 1, vv.clone(), None, None))
        .unwrap();
    h.db.repo()
        .put_entry(&h.record("gone.txt", EntryContent::Deleted, 2, vv, None, None))
        .unwrap();
    assert_eq!(h.db.repo().count_live(h.mount.id).unwrap(), 1);
    assert_eq!(h.db.repo().count_live(MountId::new()).unwrap(), 0);
}

#[test]
fn peers_shares_offers_and_progress() {
    let h = Harness::new();
    let peer_dev = device(9, "laptop");
    let peer =
        h.db.repo()
            .add_peer(&peer_dev, &["127.0.0.1:47321".into()], 5_000)
            .unwrap();
    assert_eq!(peer.device.name, "laptop");
    assert_eq!(h.db.repo().list_peers().unwrap().len(), 1);
    assert!(h.db.repo().peer_by_name("laptop").unwrap().is_some());
    assert!(h.db.repo().peer_by_id(peer_dev.id).unwrap().is_some());

    h.db.repo().share_space(h.space.id, peer_dev.id).unwrap();
    assert!(h.db.repo().is_shared(h.space.id, peer_dev.id).unwrap());
    assert_eq!(
        h.db.repo().shared_space_ids(peer_dev.id).unwrap(),
        vec![h.space.id]
    );

    let offer = crate::PeerOfferRow {
        space_id: SpaceId::new(),
        name: "Work".into(),
        mounts: vec![crate::OfferedMount {
            id: MountId::new(),
            name: "mods".into(),
        }],
    };
    h.db.repo()
        .replace_peer_offers(peer_dev.id, std::slice::from_ref(&offer), 6_000)
        .unwrap();
    let listed = h.db.repo().list_offers().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "Work");
    h.db.repo()
        .replace_peer_offers(peer_dev.id, &[], 7_000)
        .unwrap();
    assert!(h.db.repo().list_offers().unwrap().is_empty());

    h.db.repo()
        .set_received_seq(peer_dev.id, h.space.id, Sequence(12), 8_000)
        .unwrap();
    h.db.repo()
        .set_acked_seq(peer_dev.id, h.space.id, Sequence(4), 8_100)
        .unwrap();
    let progress = h.db.repo().sync_progress(peer_dev.id, h.space.id).unwrap();
    assert_eq!(progress.received_seq, Sequence(12));
    assert_eq!(progress.acked_seq, Sequence(4));
    h.db.repo()
        .reset_received_seq_for_space(h.space.id)
        .unwrap();
    let progress = h.db.repo().sync_progress(peer_dev.id, h.space.id).unwrap();
    assert_eq!(progress.received_seq, Sequence(0));
    assert_eq!(progress.acked_seq, Sequence(4));

    assert!(h.db.repo().remove_peer_by_name("laptop").unwrap());
    assert!(h.db.repo().list_peers().unwrap().is_empty());
    assert!(!h.db.repo().is_shared(h.space.id, peer_dev.id).unwrap());
    assert_eq!(
        h.db.repo()
            .sync_progress(peer_dev.id, h.space.id)
            .unwrap()
            .received_seq,
        Sequence(0)
    );
}

#[test]
fn changes_since_in_space_filters_by_mount_space() {
    let h = Harness::new();
    let other = space("Other");
    let other_mount = mount(other.id, "docs");
    h.db.repo().create_space(&other, 1_000).unwrap();
    h.db.repo().create_mount(&other_mount, 1_000).unwrap();

    let vv = vector(&[(1, 1)]);
    h.db.repo()
        .put_entry(&h.record("a.txt", file(b"a", false), 1, vv.clone(), None, None))
        .unwrap();
    let other_rec = EntryRecord {
        key: key(other.id, other_mount.id, "b.txt"),
        content: file(b"b", false),
        vector: vv,
        parent_object: None,
        sequence: Sequence(2),
        modified_by: h.local.id,
        modified_at_unix_ms: 2_000,
        stat: None,
    };
    h.db.repo().put_entry(&other_rec).unwrap();

    let in_personal =
        h.db.repo()
            .changes_since_in_space(h.space.id, Sequence(0), 100)
            .unwrap();
    assert_eq!(in_personal.len(), 1);
    assert_eq!(in_personal[0].key.path.as_str(), "a.txt");

    let in_other =
        h.db.repo()
            .changes_since_in_space(other.id, Sequence(0), 100)
            .unwrap();
    assert_eq!(in_other.len(), 1);
    assert_eq!(in_other[0].key.path.as_str(), "b.txt");

    assert_eq!(
        h.db.repo().max_sequence_in_space(h.space.id).unwrap(),
        Sequence(1)
    );
}

fn arb_vector() -> impl Strategy<Value = VersionVector> {
    proptest::collection::btree_map(0u8..8, 1u64..10_000, 0..8).prop_map(|map| {
        map.into_iter()
            .map(|(d, c)| (DeviceId::from_bytes([d; 32]), c))
            .collect()
    })
}

proptest! {
    #[test]
    fn version_vector_round_trips_through_put_entry(vector in arb_vector()) {
        let h = Harness::new();
        let record = h.record(
            "prop.txt",
            file(b"payload", false),
            1,
            vector,
            None,
            None,
        );
        h.db.repo().put_entry(&record).unwrap();
        let got = h.db.repo().entry(&record.key).unwrap().unwrap();
        prop_assert_eq!(got.vector, record.vector);
        prop_assert_eq!(got.content, record.content);
    }
}
