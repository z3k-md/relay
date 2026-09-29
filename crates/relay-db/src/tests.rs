use std::path::PathBuf;

use proptest::prelude::*;
use relay_core::{
    Device, DeviceId, EntryContent, EntryKey, EntryRecord, LogicalPath, Mount, MountId, ObjectId,
    Sequence, Space, SpaceId, StatHint, VersionVector,
};

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
fn migrations_are_idempotent_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("relay.sqlite");
    {
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 1);
    }
    {
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 1);
        db.repo().init_local_device(&device(9, "again"), 1).unwrap();
        assert_eq!(db.schema_version().unwrap(), 1);
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
            supported: 1
        }
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

    let unknown: Vec<_> = repo
        .list_devices()
        .unwrap()
        .into_iter()
        .filter(|d| d.name == "unknown")
        .map(|d| d.id)
        .collect();
    assert!(unknown.contains(&DeviceId::from_bytes([2; 32])));
    assert!(unknown.contains(&DeviceId::from_bytes([3; 32])));

    let updated = h.record(
        "src/a.rs",
        file(b"hello2", false),
        5,
        vector(&[(1, 11), (2, 20), (3, 30)]),
        Some(StatHint {
            size: 6,
            mtime_ns: 2,
            file_id: Some(7),
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
        }),
        None,
    );
    h.db.repo().put_entry(&rec).unwrap();
    let new_stat = StatHint {
        size: 3,
        mtime_ns: 99,
        file_id: Some(2),
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
