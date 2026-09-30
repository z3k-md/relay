//! Durable mailbox catch-up without a live QUIC session (D29).

use std::fs;
use std::path::Path;

use relay_core::ObjectId;
use relay_engine::{Engine, EngineConfig, ScanOptions, SyncInput, Syncer};
use relay_proto::{WireEntry, WireFile, frame, wire_entry};
use relay_replica::{DurableReplica, FsReplica};
use tempfile::TempDir;

fn init(home: &Path, name: &str) -> Engine {
    Engine::init(home, name).unwrap().with_config(EngineConfig {
        racy_window: std::time::Duration::ZERO,
    })
}

fn write_tree(root: &Path, files: &[(&str, &[u8])]) {
    for (path, bytes) in files {
        let dest = root.join(path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(dest, bytes).unwrap();
    }
}

fn seed_offer(from: &mut Engine, into: &mut Engine) {
    let wire = from.space_offers_for_peer(into.device().id).unwrap();
    let peer = from.device().id;
    let mut syncer = Syncer::new();
    syncer
        .handle(
            into,
            SyncInput::PeerConnected {
                peer,
                name: from.device().name.clone(),
            },
            &mut |_| {},
        )
        .unwrap();
    syncer
        .handle(
            into,
            SyncInput::Frame {
                peer,
                body: frame::Body::SpaceOffers(wire),
            },
            &mut |_| {},
        )
        .unwrap();
    // Leave disconnected — mailbox catch-up must not need QUIC.
    let _ = syncer.handle(into, SyncInput::PeerDisconnected { peer }, &mut |_| {});
}

#[test]
fn mailbox_catchup_without_quic() {
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let mount_a = TempDir::new().unwrap();
    let mount_b = TempDir::new().unwrap();
    let mailbox = TempDir::new().unwrap();

    let mut a = init(home_a.path(), "alpha");
    let mut b = init(home_b.path(), "bravo");
    a.add_peer("bravo", b.device().id, &["127.0.0.1:47321".into()])
        .unwrap();
    b.add_peer("alpha", a.device().id, &["127.0.0.1:47321".into()])
        .unwrap();

    a.set_replica_path(mailbox.path()).unwrap();
    b.set_replica_path(mailbox.path()).unwrap();

    a.create_space("Personal").unwrap();
    a.add_mount("Personal", "code", mount_a.path(), &[], &[])
        .unwrap();
    write_tree(mount_a.path(), &[("note.txt", b"from-a")]);
    a.scan("Personal", "code", ScanOptions::default()).unwrap();
    a.share("Personal", "bravo").unwrap();

    seed_offer(&mut a, &mut b);
    b.join_space("Personal", "alpha").unwrap();
    b.add_mount("Personal", "code", mount_b.path(), &[], &[])
        .unwrap();

    let push = a.push_replica().unwrap();
    assert!(push.entries > 0, "{push:?}");
    assert!(push.objects > 0, "{push:?}");

    let pull = b.pull_replica().unwrap();
    assert!(pull.entries > 0, "{pull:?}");
    assert_eq!(
        fs::read(mount_b.path().join("note.txt")).unwrap(),
        b"from-a"
    );

    write_tree(mount_b.path(), &[("note.txt", b"from-b-edited")]);
    b.scan("Personal", "code", ScanOptions::default()).unwrap();
    b.push_replica().unwrap();
    a.pull_replica().unwrap();
    assert_eq!(
        fs::read(mount_a.path().join("note.txt")).unwrap(),
        b"from-b-edited"
    );
}

#[test]
fn missing_mailbox_object_holds_received_cursor() {
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let mount_a = TempDir::new().unwrap();
    let mount_b = TempDir::new().unwrap();
    let mailbox = TempDir::new().unwrap();

    let mut a = init(home_a.path(), "alpha");
    let mut b = init(home_b.path(), "bravo");
    a.add_peer("bravo", b.device().id, &["127.0.0.1:47321".into()])
        .unwrap();
    b.add_peer("alpha", a.device().id, &["127.0.0.1:47321".into()])
        .unwrap();
    a.set_replica_path(mailbox.path()).unwrap();
    b.set_replica_path(mailbox.path()).unwrap();

    a.create_space("Personal").unwrap();
    a.add_mount("Personal", "code", mount_a.path(), &[], &[])
        .unwrap();
    write_tree(mount_a.path(), &[("first.txt", b"one")]);
    a.scan("Personal", "code", ScanOptions::default()).unwrap();
    a.share("Personal", "bravo").unwrap();
    seed_offer(&mut a, &mut b);
    b.join_space("Personal", "alpha").unwrap();
    b.add_mount("Personal", "code", mount_b.path(), &[], &[])
        .unwrap();

    a.push_replica().unwrap();

    let space = a.spaces().unwrap().into_iter().next().unwrap();
    let mounts = a.mounts(Some("Personal")).unwrap();
    let mount_id = mounts[0].1.mount.id;
    let author = a.device().id;
    let missing_bytes = b"missing-object-bytes";
    let missing_id = ObjectId::of(missing_bytes);
    let entries = a.entries("Personal", "code", true).unwrap();
    let last_seq = entries.iter().map(|e| e.sequence.0).max().unwrap_or(0);
    let bad = WireEntry {
        mount_id: mount_id.as_uuid().as_bytes().to_vec(),
        path: "second.txt".into(),
        content: Some(wire_entry::Content::File(WireFile {
            object: missing_id.as_bytes().to_vec(),
            size: missing_bytes.len() as u64,
            executable: false,
        })),
        vector: vec![relay_proto::Counter {
            device_id: author.as_bytes().to_vec(),
            value: 1,
        }],
        parent_object: None,
        modified_by: author.as_bytes().to_vec(),
        modified_at_unix_ms: 1,
        sequence: last_seq + 1,
        mtime_unix_ns: None,
    };
    let mut replica = FsReplica::open(mailbox.path()).unwrap();
    replica.append_entries(author, space.id, &[bad]).unwrap();

    let before = b
        .status()
        .unwrap()
        .peers
        .iter()
        .find(|p| p.name == "alpha")
        .and_then(|p| p.spaces.iter().find(|s| s.space == "Personal"))
        .map(|s| s.received_seq.0)
        .unwrap_or(0);

    b.pull_replica().unwrap();
    assert_eq!(fs::read(mount_b.path().join("first.txt")).unwrap(), b"one");
    assert!(!mount_b.path().join("second.txt").exists());

    let after = b
        .status()
        .unwrap()
        .peers
        .iter()
        .find(|p| p.name == "alpha")
        .and_then(|p| p.spaces.iter().find(|s| s.space == "Personal"))
        .map(|s| s.received_seq.0)
        .unwrap_or(0);
    assert!(after > before, "complete prefix should advance received");
    assert_eq!(
        after, last_seq,
        "received must stop before the missing-object entry (last_seq={last_seq}, after={after})"
    );

    replica.put_object(missing_id, missing_bytes).unwrap();
    b.pull_replica().unwrap();
    assert_eq!(
        fs::read(mount_b.path().join("second.txt")).unwrap(),
        missing_bytes
    );
}
