//! Deterministic in-process two-engine sync harness.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use relay_core::conflict::conflict_path;
use relay_core::{DeviceId, EntryContent, LogicalPath, MOUNT_MARKER, ObjectId, SpaceId};
use relay_engine::{
    Clock, DeleteHoldDecision, Engine, EngineConfig, ManualClock, MaterializationMode, ScanOptions,
    SyncEvent, SyncInput, SyncOutput, Syncer, TransferDirection,
};
use relay_fs::MountMarker;
use relay_proto::{IndexBatch, entry_to_wire, frame, mount_id_bytes, space_id_bytes};
use relay_replica::{DurableReplica, FsReplica};
use tempfile::TempDir;

fn init(home: &Path, name: &str, racy_window: std::time::Duration) -> Engine {
    Engine::init(home, name)
        .unwrap()
        .with_config(EngineConfig { racy_window })
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

fn index_triples(engine: &Engine, space: &str, mount: &str) -> BTreeSet<(String, String, String)> {
    engine
        .entries(space, mount, true)
        .unwrap()
        .into_iter()
        .map(|e| {
            (
                e.key.path.to_string(),
                format!("{:?}", e.content),
                format!("{:?}", e.vector),
            )
        })
        .collect()
}

fn live_files(root: &Path) -> BTreeSet<(String, Vec<u8>)> {
    let mut out = BTreeSet::new();
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<(String, Vec<u8>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".relay-") {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert((rel, fs::read(&path).unwrap()));
            }
        }
    }
    walk(root, root, &mut out);
    out
}

struct Harness {
    _home_a: TempDir,
    _home_b: TempDir,
    mount_a: TempDir,
    mount_b: TempDir,
    a: Engine,
    b: Engine,
    sa: Syncer,
    sb: Syncer,
    /// Fail on any sync warning (skipped entry, bounded re-evaluation, ...).
    strict: bool,
    /// Objects whose fetches fail (as a transient error) while present here.
    broken_objects: HashSet<ObjectId>,
    events: Vec<SyncEvent>,
    /// Object ids any side asked to fetch during `pump`.
    fetches: Vec<ObjectId>,
}

impl Harness {
    fn pair() -> Self {
        Self::pair_with_racy_window(std::time::Duration::ZERO)
    }

    /// A huge racy window makes every recorded stat `None`, as for files
    /// edited moments before a remote update arrives.
    fn pair_with_racy_window(racy_window: std::time::Duration) -> Self {
        let home_a = TempDir::new().unwrap();
        let home_b = TempDir::new().unwrap();
        let mount_a = TempDir::new().unwrap();
        let mount_b = TempDir::new().unwrap();
        let a = init(home_a.path(), "alpha", racy_window);
        let b = init(home_b.path(), "bravo", racy_window);
        Self {
            _home_a: home_a,
            _home_b: home_b,
            mount_a,
            mount_b,
            a,
            b,
            sa: Syncer::new(),
            sb: Syncer::new(),
            strict: true,
            broken_objects: HashSet::new(),
            events: Vec::new(),
            fetches: Vec::new(),
        }
    }

    fn id_a(&self) -> DeviceId {
        self.a.device().id
    }
    fn id_b(&self) -> DeviceId {
        self.b.device().id
    }

    fn pair_peers(&mut self) {
        self.a
            .add_peer("bravo", self.id_b(), &["127.0.0.1:47321".into()])
            .unwrap();
        self.b
            .add_peer("alpha", self.id_a(), &["127.0.0.1:47321".into()])
            .unwrap();
    }

    fn connect(&mut self) {
        let mut q = VecDeque::new();
        q.push_back((
            true,
            SyncInput::PeerConnected {
                peer: self.id_b(),
                name: "bravo".into(),
            },
        ));
        q.push_back((
            false,
            SyncInput::PeerConnected {
                peer: self.id_a(),
                name: "alpha".into(),
            },
        ));
        self.pump(q, None);
    }

    fn disconnect(&mut self) {
        let id_a = self.id_a();
        let id_b = self.id_b();
        let _ = self.sa.handle(
            &mut self.a,
            SyncInput::PeerDisconnected { peer: id_b },
            &mut |_| {},
        );
        let _ = self.sb.handle(
            &mut self.b,
            SyncInput::PeerDisconnected { peer: id_a },
            &mut |_| {},
        );
    }

    fn drive(&mut self, input: SyncInput, to_a: bool) {
        let mut q = VecDeque::new();
        q.push_back((to_a, input));
        self.pump(q, None);
    }

    fn tick_b(&mut self, now: std::time::Instant) {
        let mut outs = Vec::new();
        let events = self
            .sb
            .tick(&mut self.b, now, &mut |o| outs.push(o))
            .unwrap();
        assert_no_warnings(&events, self.strict);
        self.events.extend(events);
        let mut q = VecDeque::new();
        for output in outs {
            match output {
                SyncOutput::Send { body, .. } => {
                    q.push_back((
                        true,
                        SyncInput::Frame {
                            peer: self.id_b(),
                            body,
                        },
                    ));
                }
                SyncOutput::FetchObject { object, .. } => {
                    self.fetches.push(object);
                    let input = if self.broken_objects.contains(&object) {
                        SyncInput::ObjectFetchFailed {
                            peer: self.id_a(),
                            object,
                            not_found: false,
                            reason: "simulated I/O error".into(),
                        }
                    } else {
                        copy_object(&self.a, &self.b, self.id_a(), object)
                    };
                    q.push_back((false, input));
                }
                SyncOutput::SetPeers | SyncOutput::SetRelay(_) => {}
            }
        }
        self.pump(q, None);
    }

    fn push_both(&mut self) {
        self.pump(VecDeque::new(), Some(true));
    }

    fn reconnect(&mut self) {
        self.disconnect();
        self.sa = Syncer::new();
        self.sb = Syncer::new();
        self.connect();
        self.push_both();
    }

    fn received(&self, from_a: bool) -> u64 {
        let (engine, name) = if from_a {
            (&self.a, "bravo")
        } else {
            (&self.b, "alpha")
        };
        engine
            .status()
            .unwrap()
            .peers
            .iter()
            .find(|p| p.name == name)
            .and_then(|p| p.spaces.iter().find(|s| s.space == "Personal"))
            .map(|s| s.received_seq.0)
            .unwrap_or(0)
    }

    fn pump(&mut self, mut q: VecDeque<(bool, SyncInput)>, push: Option<bool>) {
        let mut outputs: VecDeque<(bool, SyncOutput)> = VecDeque::new();
        let mut steps = 0;
        if push == Some(true) {
            collect_push(
                &mut self.sa,
                &mut self.a,
                true,
                &mut outputs,
                self.strict,
                &mut self.events,
            );
            collect_push(
                &mut self.sb,
                &mut self.b,
                false,
                &mut outputs,
                self.strict,
                &mut self.events,
            );
        }
        loop {
            steps += 1;
            assert!(steps < 50_000, "sync pump did not go quiet");
            if let Some((to_a, input)) = q.pop_front() {
                let (outs, ev) = if to_a {
                    take_handle(&mut self.sa, &mut self.a, input, self.strict)
                } else {
                    take_handle(&mut self.sb, &mut self.b, input, self.strict)
                };
                self.events.extend(ev);
                for o in outs {
                    outputs.push_back((to_a, o));
                }
                continue;
            }
            let Some((from_a, output)) = outputs.pop_front() else {
                break;
            };
            match output {
                SyncOutput::Send { peer, body } => {
                    let to_a = peer == self.id_a();
                    assert_eq!(to_a, !from_a, "send went to the wrong engine");
                    q.push_back((
                        to_a,
                        SyncInput::Frame {
                            peer: if from_a { self.id_a() } else { self.id_b() },
                            body,
                        },
                    ));
                }
                SyncOutput::SetPeers | SyncOutput::SetRelay(_) => {}
                SyncOutput::FetchObject { object, .. } => {
                    self.fetches.push(object);
                    // The engine that emitted FetchObject is the requester.
                    let (src, dst, from_peer) = if from_a {
                        (&self.b, &self.a, self.id_b())
                    } else {
                        (&self.a, &self.b, self.id_a())
                    };
                    let input = if self.broken_objects.contains(&object) {
                        SyncInput::ObjectFetchFailed {
                            peer: from_peer,
                            object,
                            not_found: false,
                            reason: "simulated I/O error".into(),
                        }
                    } else {
                        copy_object(src, dst, from_peer, object)
                    };
                    q.push_back((from_a, input));
                }
            }
        }
    }

    fn setup_shared_space(&mut self, files: &[(&str, &[u8])]) {
        self.pair_peers();
        self.a.create_space("Personal").unwrap();
        self.a
            .add_mount("Personal", "code", self.mount_a.path(), &[], &[])
            .unwrap();
        write_tree(self.mount_a.path(), files);
        self.a
            .scan("Personal", "code", ScanOptions::default())
            .unwrap();
        self.a.share("Personal", "bravo").unwrap();
        self.connect();
        self.pump(VecDeque::new(), None);
        self.b.join_space("Personal", "alpha").unwrap();
        self.b
            .add_mount("Personal", "code", self.mount_b.path(), &[], &[])
            .unwrap();
        self.disconnect();
        self.connect();
        self.pump(VecDeque::new(), None);
        let _ = self.sa.push_local_changes(&mut self.a, &mut |_| {});
        self.push_both();
    }
}

#[test]
fn catchup_plan_is_stamped_on_every_batch() {
    let mut h = Harness::pair();
    h.sa = Syncer::with_index_batch_entries(1);
    h.setup_shared_space(&[]);
    h.disconnect();
    write_tree(
        h.mount_a.path(),
        &[("a.txt", b"aa"), ("b.txt", b"bbb"), ("c.txt", b"cccc")],
    );
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();

    let id_a = h.id_a();
    let id_b = h.id_b();
    let mut from_b = Vec::new();
    h.sb.handle(
        &mut h.b,
        SyncInput::PeerConnected {
            peer: id_a,
            name: "alpha".into(),
        },
        &mut |output| from_b.push(output),
    )
    .unwrap();
    h.sa.handle(
        &mut h.a,
        SyncInput::PeerConnected {
            peer: id_b,
            name: "bravo".into(),
        },
        &mut |_| {},
    )
    .unwrap();

    let mut batches = Vec::new();
    for output in from_b {
        let SyncOutput::Send { body, .. } = output else {
            continue;
        };
        h.sa.handle(
            &mut h.a,
            SyncInput::Frame { peer: id_b, body },
            &mut |output| batches.push(output),
        )
        .unwrap();
    }
    let plans: Vec<_> = batches
        .iter()
        .filter_map(|output| match output {
            SyncOutput::Send {
                body: frame::Body::IndexBatch(batch),
                ..
            } => Some((batch.plan_files, batch.plan_bytes, batch.plan_after)),
            _ => None,
        })
        .collect();
    assert!(
        plans.len() >= 3,
        "expected one batch per file, got {plans:?}"
    );
    assert!(plans.iter().all(|plan| *plan == plans[0]));
    let (files, bytes, _) = plans[0];
    assert!(files.is_some_and(|n| n >= 3));
    assert_eq!(bytes, Some(2 + 3 + 4));
}

#[test]
fn local_object_counts_and_both_rows_finish() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[]);
    h.disconnect();
    write_tree(h.mount_a.path(), &[("a.txt", b"aa"), ("b.txt", b"bbb")]);
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.b.store().put_bytes(b"aa").unwrap();
    h.events.clear();
    h.connect();
    h.pump(VecDeque::new(), Some(true));

    let transfers: Vec<&Vec<_>> = h
        .events
        .iter()
        .filter_map(|event| match event {
            SyncEvent::Transfers(rows) => Some(rows),
            _ => None,
        })
        .collect();
    let receive: Vec<_> = transfers
        .iter()
        .flat_map(|rows| rows.iter())
        .filter(|row| row.direction == TransferDirection::Receive)
        .collect();
    assert!(
        receive
            .iter()
            .any(|row| row.bytes_total == Some(5) && row.bytes_done == 2),
        "local object should count before the other file arrives: {receive:?}"
    );
    assert!(
        receive
            .iter()
            .any(|row| row.bytes_total == Some(5) && row.bytes_done == 5),
        "receive row should reach the plan: {receive:?}"
    );
    let send: Vec<_> = transfers
        .iter()
        .flat_map(|rows| rows.iter())
        .filter(|row| row.direction == TransferDirection::Send)
        .collect();
    assert!(
        send.iter()
            .any(|row| row.bytes_total.is_none() && row.bytes_done < 5),
        "send row has no byte percent and stays under the plan: {send:?}"
    );
    assert!(
        h.sa.live_transfers().is_empty() && h.sb.live_transfers().is_empty(),
        "rows should close once the receive is applied and the send is acked"
    );
    assert_eq!(
        index_triples(&h.b, "Personal", "code"),
        index_triples(&h.a, "Personal", "code")
    );
}

fn assert_no_warnings(events: &[SyncEvent], strict: bool) {
    if !strict {
        return;
    }
    for event in events {
        if let SyncEvent::SyncWarning { path, reason, .. } = event {
            panic!("unexpected sync warning for {path}: {reason}");
        }
        if let SyncEvent::RemoteApplied { skipped, .. } = event {
            assert_eq!(*skipped, 0, "unexpected skipped entries: {events:?}");
        }
    }
}

fn take_handle(
    syncer: &mut Syncer,
    engine: &mut Engine,
    input: SyncInput,
    strict: bool,
) -> (Vec<SyncOutput>, Vec<SyncEvent>) {
    let mut outs = Vec::new();
    let events = syncer.handle(engine, input, &mut |o| outs.push(o)).unwrap();
    assert_no_warnings(&events, strict);
    (outs, events)
}

fn collect_push(
    syncer: &mut Syncer,
    engine: &mut Engine,
    _from_a: bool,
    outputs: &mut VecDeque<(bool, SyncOutput)>,
    strict: bool,
    collected: &mut Vec<SyncEvent>,
) {
    let mut outs = Vec::new();
    let events = syncer
        .push_local_changes(engine, &mut |o| outs.push(o))
        .unwrap();
    assert_no_warnings(&events, strict);
    collected.extend(events);
    for o in outs {
        outputs.push_back((_from_a, o));
    }
}

fn copy_object(src: &Engine, dst: &Engine, from_peer: DeviceId, object: ObjectId) -> SyncInput {
    if !src.store().contains(&object) {
        return SyncInput::ObjectFetchFailed {
            peer: from_peer,
            object,
            not_found: true,
            reason: "not found".into(),
        };
    }
    let bytes = src.store().read(&object).unwrap();
    assert_eq!(ObjectId::of(&bytes), object, "store hash mismatch");
    dst.store().put_bytes(&bytes).unwrap();
    SyncInput::ObjectFetched {
        peer: from_peer,
        object,
    }
}

#[test]
fn pair_share_join_attach_and_echo_suppression() {
    let mut h = Harness::pair();
    let files: &[(&str, &[u8])] = &[
        ("nested/dir/a.lua", b"print(1)"),
        ("nested/dir/b.txt", b"hello"),
        (".git/objects/ab/cd", b"blob"),
        (".git/HEAD", b"ref: refs/heads/main\n"),
        (".git/refs/heads/main", b"abc123\n"),
        (".git/index", b"DIRC"),
    ];
    h.setup_shared_space(files);
    assert_eq!(live_files(h.mount_a.path()), live_files(h.mount_b.path()));
    let report =
        h.b.scan("Personal", "code", ScanOptions::default())
            .unwrap();
    assert_eq!(report.created, 0, "{report:?}");
    assert_eq!(report.modified, 0, "{report:?}");
    assert_eq!(report.deleted, 0, "{report:?}");
    assert_eq!(
        index_triples(&h.a, "Personal", "code"),
        index_triples(&h.b, "Personal", "code")
    );
}

#[test]
fn modify_and_delete_propagate_both_ways() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("a.txt", b"v1"), ("b.txt", b"keep"), ("z.txt", b"stay")]);
    fs::write(h.mount_a.path().join("a.txt"), b"v2").unwrap();
    fs::remove_file(h.mount_a.path().join("b.txt")).unwrap();
    h.a.scan(
        "Personal",
        "code",
        ScanOptions {
            allow_mass_delete: true,
            ..ScanOptions::default()
        },
    )
    .unwrap();
    h.push_both();
    assert_eq!(fs::read(h.mount_b.path().join("a.txt")).unwrap(), b"v2");
    assert!(!h.mount_b.path().join("b.txt").exists());

    fs::write(h.mount_b.path().join("c.txt"), b"from-b").unwrap();
    h.b.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    assert_eq!(fs::read(h.mount_a.path().join("c.txt")).unwrap(), b"from-b");
}

#[test]
fn nonoverlapping_text_edits_merge_without_a_conflict_copy() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("foo.txt", b"alpha\nbeta\n")]);
    fs::write(h.mount_a.path().join("foo.txt"), b"ALPHA\nbeta\n").unwrap();
    fs::write(h.mount_b.path().join("foo.txt"), b"alpha\nBETA\n").unwrap();
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.b.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    h.push_both();
    assert!(h.a.conflicts(None).unwrap().is_empty());
    assert!(h.b.conflicts(None).unwrap().is_empty());
    let merged = b"ALPHA\nBETA\n";
    assert_eq!(fs::read(h.mount_a.path().join("foo.txt")).unwrap(), merged);
    assert_eq!(fs::read(h.mount_b.path().join("foo.txt")).unwrap(), merged);
    assert_eq!(
        index_triples(&h.a, "Personal", "code"),
        index_triples(&h.b, "Personal", "code")
    );
}

#[test]
fn concurrent_edits_same_winner_and_conflict_copy() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("foo.go", b"base")]);
    fs::write(h.mount_a.path().join("foo.go"), b"alpha").unwrap();
    fs::write(h.mount_b.path().join("foo.go"), b"bravo").unwrap();
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.b.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    h.push_both();
    assert_eq!(
        index_triples(&h.a, "Personal", "code"),
        index_triples(&h.b, "Personal", "code")
    );
    let conflicts_a = h.a.conflicts(None).unwrap();
    let conflicts_b = h.b.conflicts(None).unwrap();
    assert_eq!(conflicts_a.len(), 1);
    assert_eq!(conflicts_b.len(), 1);
    assert_eq!(conflicts_a[0].key.path, conflicts_b[0].key.path);
    assert_eq!(conflicts_a[0].content, conflicts_b[0].content);
    assert_eq!(
        fs::read(h.mount_a.path().join("foo.go")).unwrap(),
        fs::read(h.mount_b.path().join("foo.go")).unwrap()
    );
    let before = index_triples(&h.a, "Personal", "code");
    h.disconnect();
    h.connect();
    h.push_both();
    assert_eq!(before, index_triples(&h.a, "Personal", "code"));
    assert_eq!(before, index_triples(&h.b, "Personal", "code"));
}

#[test]
fn concurrent_edits_of_just_modified_files_resolve_on_both_sides() {
    for racy_window in [
        std::time::Duration::ZERO,
        std::time::Duration::from_secs(3600),
    ] {
        let mut h = Harness::pair_with_racy_window(racy_window);
        h.setup_shared_space(&[("foo.go", b"base"), ("dir/x", b"x")]);
        fs::write(h.mount_a.path().join("foo.go"), b"alpha").unwrap();
        fs::write(h.mount_b.path().join("foo.go"), b"bravo").unwrap();
        h.a.scan("Personal", "code", ScanOptions::default())
            .unwrap();
        h.b.scan("Personal", "code", ScanOptions::default())
            .unwrap();
        h.push_both();
        h.push_both();
        assert_eq!(live_files(h.mount_a.path()), live_files(h.mount_b.path()));
        let files = live_files(h.mount_a.path());
        let contents: BTreeSet<&[u8]> = files.iter().map(|(_, b)| b.as_slice()).collect();
        assert!(
            contents.contains(&b"alpha"[..]),
            "{racy_window:?}: {files:?}"
        );
        assert!(
            contents.contains(&b"bravo"[..]),
            "{racy_window:?}: {files:?}"
        );
        assert_eq!(
            index_triples(&h.a, "Personal", "code"),
            index_triples(&h.b, "Personal", "code")
        );
    }
}

#[test]
fn failed_fetches_are_re_requested_instead_of_dropped() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("keep.txt", b"k")]);
    h.strict = false;
    let broken: &[u8] = b"cannot be fetched yet";
    fs::write(h.mount_a.path().join("big.bin"), broken).unwrap();
    fs::write(h.mount_a.path().join("ok.txt"), b"fine").unwrap();
    h.broken_objects.insert(ObjectId::of(broken));
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    assert_eq!(fs::read(h.mount_b.path().join("ok.txt")).unwrap(), b"fine");
    assert!(!h.mount_b.path().join("big.bin").exists());

    // Still failing when the re-request fires: nothing is lost, still pending.
    let later = std::time::Instant::now() + std::time::Duration::from_secs(31);
    h.tick_b(later);
    assert!(!h.mount_b.path().join("big.bin").exists());

    // Fault clears; the next re-request delivers the file.
    h.broken_objects.clear();
    h.tick_b(later + std::time::Duration::from_secs(31));
    assert_eq!(fs::read(h.mount_b.path().join("big.bin")).unwrap(), broken);

    // And a restart would not have lost it either: the watermark held.
    h.strict = true;
    h.disconnect();
    h.connect();
    h.push_both();
    assert_eq!(
        index_triples(&h.a, "Personal", "code"),
        index_triples(&h.b, "Personal", "code")
    );
}

#[test]
fn fetch_failure_asks_other_peer_then_mailbox() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[]);
    h.disconnect();

    let bytes = b"from-mailbox";
    let object = ObjectId::of(bytes);
    write_tree(h.mount_a.path(), &[("note.txt", bytes)]);
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();

    let id_c = DeviceId::random();
    h.b.add_peer("charlie", id_c, &["127.0.0.1:9".into()])
        .unwrap();
    h.b.share("Personal", "charlie").unwrap();

    let mailbox = TempDir::new().unwrap();
    h.b.set_replica_path(mailbox.path()).unwrap();
    let path = h.b.replica_path().unwrap().unwrap();
    let mut replica = FsReplica::open(&path).unwrap();
    replica.put_object(object, bytes).unwrap();

    let id_a = h.id_a();
    let id_b = h.id_b();
    let mut from_b = Vec::new();
    let events =
        h.sb.handle(
            &mut h.b,
            SyncInput::PeerConnected {
                peer: id_a,
                name: "alpha".into(),
            },
            &mut |output| from_b.push(output),
        )
        .unwrap();
    assert_no_warnings(&events, true);
    let events =
        h.sb.handle(
            &mut h.b,
            SyncInput::PeerConnected {
                peer: id_c,
                name: "charlie".into(),
            },
            &mut |_| {},
        )
        .unwrap();
    assert_no_warnings(&events, true);
    let events =
        h.sa.handle(
            &mut h.a,
            SyncInput::PeerConnected {
                peer: id_b,
                name: "bravo".into(),
            },
            &mut |_| {},
        )
        .unwrap();
    assert_no_warnings(&events, true);

    let mut from_a = Vec::new();
    for output in from_b {
        let SyncOutput::Send { body, peer } = output else {
            continue;
        };
        if peer != id_a {
            continue;
        }
        let frame::Body::IndexRequest(_) = &body else {
            continue;
        };
        let events =
            h.sa.handle(
                &mut h.a,
                SyncInput::Frame { peer: id_b, body },
                &mut |output| from_a.push(output),
            )
            .unwrap();
        assert_no_warnings(&events, true);
    }

    let mut fetches = Vec::new();
    for output in from_a {
        let SyncOutput::Send { body, peer } = output else {
            continue;
        };
        if peer != id_b {
            continue;
        }
        let events =
            h.sb.handle(
                &mut h.b,
                SyncInput::Frame { peer: id_a, body },
                &mut |output| fetches.push(output),
            )
            .unwrap();
        assert_no_warnings(&events, true);
    }

    let initial: Vec<DeviceId> = fetches
        .iter()
        .filter_map(|output| match output {
            SyncOutput::FetchObject { peer, object: id } if *id == object => Some(*peer),
            SyncOutput::FetchObject { .. } => panic!("unexpected fetch: {output:?}"),
            _ => None,
        })
        .collect();
    assert_eq!(
        initial,
        vec![id_a],
        "first fetch stays with the index source"
    );

    let mut next = Vec::new();
    let events =
        h.sb.handle(
            &mut h.b,
            SyncInput::ObjectFetchFailed {
                peer: id_a,
                object,
                not_found: true,
                reason: "not found".into(),
            },
            &mut |output| next.push(output),
        )
        .unwrap();
    assert_no_warnings(&events, true);
    let asked: Vec<DeviceId> = next
        .iter()
        .filter_map(|output| match output {
            SyncOutput::FetchObject { peer, object: id } if *id == object => Some(*peer),
            SyncOutput::FetchObject { .. } => panic!("unexpected fetch: {output:?}"),
            _ => None,
        })
        .collect();
    assert_eq!(asked, vec![id_c]);
    assert!(!h.b.store().contains(&object));
    assert!(!h.mount_b.path().join("note.txt").exists());

    let mut done = Vec::new();
    let events =
        h.sb.handle(
            &mut h.b,
            SyncInput::ObjectFetchFailed {
                peer: id_c,
                object,
                not_found: true,
                reason: "not found".into(),
            },
            &mut |output| done.push(output),
        )
        .unwrap();
    assert_no_warnings(&events, true);
    assert!(
        done.iter()
            .all(|output| !matches!(output, SyncOutput::FetchObject { .. })),
        "mailbox should satisfy the object without another fetch: {done:?}"
    );
    assert!(h.b.store().contains(&object));
    assert_eq!(h.b.store().read(&object).unwrap(), bytes);
    assert_eq!(fs::read(h.mount_b.path().join("note.txt")).unwrap(), bytes);
    assert!(
        h.b.entries("Personal", "code", true)
            .unwrap()
            .iter()
            .any(|entry| entry.key.path.as_str() == "note.txt")
    );
}

#[test]
fn alternate_not_found_still_retries_a_transient_source() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[]);
    h.disconnect();

    let bytes = b"still-on-source";
    let object = ObjectId::of(bytes);
    write_tree(h.mount_a.path(), &[("note.txt", bytes)]);
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();

    let id_c = DeviceId::random();
    h.b.add_peer("charlie", id_c, &["127.0.0.1:9".into()])
        .unwrap();
    h.b.share("Personal", "charlie").unwrap();

    let id_a = h.id_a();
    let id_b = h.id_b();
    let mut from_b = Vec::new();
    h.sb.handle(
        &mut h.b,
        SyncInput::PeerConnected {
            peer: id_a,
            name: "alpha".into(),
        },
        &mut |output| from_b.push(output),
    )
    .unwrap();
    h.sb.handle(
        &mut h.b,
        SyncInput::PeerConnected {
            peer: id_c,
            name: "charlie".into(),
        },
        &mut |_| {},
    )
    .unwrap();
    h.sa.handle(
        &mut h.a,
        SyncInput::PeerConnected {
            peer: id_b,
            name: "bravo".into(),
        },
        &mut |_| {},
    )
    .unwrap();

    let mut from_a = Vec::new();
    for output in from_b {
        let SyncOutput::Send { body, peer } = output else {
            continue;
        };
        if peer != id_a {
            continue;
        }
        let frame::Body::IndexRequest(_) = &body else {
            continue;
        };
        h.sa.handle(
            &mut h.a,
            SyncInput::Frame { peer: id_b, body },
            &mut |output| from_a.push(output),
        )
        .unwrap();
    }

    let mut fetches = Vec::new();
    for output in from_a {
        let SyncOutput::Send { body, peer } = output else {
            continue;
        };
        if peer != id_b {
            continue;
        }
        h.sb.handle(
            &mut h.b,
            SyncInput::Frame { peer: id_a, body },
            &mut |output| fetches.push(output),
        )
        .unwrap();
    }
    assert!(fetches.iter().any(|output| matches!(
        output,
        SyncOutput::FetchObject { peer, object: id } if *peer == id_a && *id == object
    )));

    let mut next = Vec::new();
    h.sb.handle(
        &mut h.b,
        SyncInput::ObjectFetchFailed {
            peer: id_a,
            object,
            not_found: false,
            reason: "reset".into(),
        },
        &mut |output| next.push(output),
    )
    .unwrap();
    assert!(next.iter().any(|output| matches!(
        output,
        SyncOutput::FetchObject { peer, object: id } if *peer == id_c && *id == object
    )));

    let mut retry = Vec::new();
    h.sb.handle(
        &mut h.b,
        SyncInput::ObjectFetchFailed {
            peer: id_c,
            object,
            not_found: true,
            reason: "not found".into(),
        },
        &mut |output| retry.push(output),
    )
    .unwrap();
    assert!(
        retry.iter().any(|output| matches!(
            output,
            SyncOutput::FetchObject { peer, object: id } if *peer == id_a && *id == object
        )),
        "a missing alternate must not cancel retries of a transient source: {retry:?}"
    );
    assert!(!h.mount_b.path().join("note.txt").exists());
}

#[test]
fn delete_vs_modify_modify_wins() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("x.txt", b"old"), ("keep.txt", b"k")]);
    fs::remove_file(h.mount_a.path().join("x.txt")).unwrap();
    fs::write(h.mount_b.path().join("x.txt"), b"new").unwrap();
    h.a.scan(
        "Personal",
        "code",
        ScanOptions {
            allow_mass_delete: true,
            ..ScanOptions::default()
        },
    )
    .unwrap();
    h.b.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    h.push_both();
    assert_eq!(fs::read(h.mount_a.path().join("x.txt")).unwrap(), b"new");
    assert_eq!(fs::read(h.mount_b.path().join("x.txt")).unwrap(), b"new");
    assert!(h.a.conflicts(None).unwrap().is_empty());
    assert!(h.b.conflicts(None).unwrap().is_empty());
}

#[test]
fn concurrent_identical_merges_without_copy() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("same.txt", b"old")]);
    fs::write(h.mount_a.path().join("same.txt"), b"same").unwrap();
    fs::write(h.mount_b.path().join("same.txt"), b"same").unwrap();
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.b.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    h.push_both();
    assert!(h.a.conflicts(None).unwrap().is_empty());
    assert!(h.b.conflicts(None).unwrap().is_empty());
    assert_eq!(
        index_triples(&h.a, "Personal", "code"),
        index_triples(&h.b, "Personal", "code")
    );
    let a =
        h.a.entries("Personal", "code", false)
            .unwrap()
            .into_iter()
            .find(|e| e.key.path.as_str() == "same.txt")
            .unwrap();
    assert!(a.vector.len() >= 2, "{:?}", a.vector);
}

#[test]
fn unscanned_local_edit_is_not_lost() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("edit.txt", b"orig")]);
    fs::write(h.mount_b.path().join("edit.txt"), b"local-b").unwrap();
    fs::write(h.mount_a.path().join("edit.txt"), b"from-a").unwrap();
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    h.push_both();
    let bytes_a: BTreeSet<_> = live_files(h.mount_a.path())
        .into_iter()
        .map(|(_, b)| b)
        .collect();
    let bytes_b: BTreeSet<_> = live_files(h.mount_b.path())
        .into_iter()
        .map(|(_, b)| b)
        .collect();
    assert!(bytes_a.iter().any(|b| b == b"from-a" || b == b"local-b"));
    assert!(bytes_b.iter().any(|b| b == b"from-a" || b == b"local-b"));
    assert!(
        bytes_a.contains(b"from-a".as_slice()) && bytes_a.contains(b"local-b".as_slice())
            || bytes_b.contains(b"from-a".as_slice()) && bytes_b.contains(b"local-b".as_slice())
            || h.a.conflicts(None).unwrap().len() + h.b.conflicts(None).unwrap().len() > 0
    );
}

#[test]
fn reconnect_after_drop_and_restart_sends_only_new_changes() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("one.txt", b"1")]);
    h.disconnect();
    h.connect();
    let mut sent = 0usize;
    let mut outs = Vec::new();
    h.sa.push_local_changes(&mut h.a, &mut |o| outs.push(o))
        .unwrap();
    for o in &outs {
        if let SyncOutput::Send {
            body: frame::Body::IndexBatch(batch),
            ..
        } = o
        {
            sent += batch.entries.len();
        }
    }
    assert_eq!(sent, 0, "restart/reconnect should not resend the backlog");

    fs::write(h.mount_a.path().join("two.txt"), b"2").unwrap();
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    let mut sent_new = 0usize;
    let mut outs = Vec::new();
    h.sa.push_local_changes(&mut h.a, &mut |o| outs.push(o))
        .unwrap();
    for o in &outs {
        if let SyncOutput::Send {
            body: frame::Body::IndexBatch(batch),
            ..
        } = o
        {
            sent_new += batch.entries.len();
        }
    }
    assert!(sent_new >= 1);
    h.pump(
        outs.into_iter()
            .filter_map(|o| match o {
                SyncOutput::Send { body, .. } => Some((
                    false,
                    SyncInput::Frame {
                        peer: h.id_a(),
                        body,
                    },
                )),
                SyncOutput::FetchObject { object, .. } => {
                    Some((false, copy_object(&h.a, &h.b, h.id_a(), object)))
                }
                SyncOutput::SetPeers | SyncOutput::SetRelay(_) => None,
            })
            .collect(),
        None,
    );
    assert_eq!(fs::read(h.mount_b.path().join("two.txt")).unwrap(), b"2");
}

#[cfg(unix)]
#[test]
fn hostile_symlink_escape_is_skipped() {
    let mut h = Harness::pair();
    h.strict = false;
    h.setup_shared_space(&[("safe.txt", b"ok")]);
    let outside = TempDir::new().unwrap();
    std::os::unix::fs::symlink(outside.path(), h.mount_b.path().join("evil")).unwrap();

    let space = h.a.spaces().unwrap().into_iter().next().unwrap();
    let _mount = h.a.mounts(Some("Personal")).unwrap()[0].1.mount.id;
    let local =
        h.a.entries("Personal", "code", false)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
    let mut wire = entry_to_wire(&local);
    wire.path = "evil/x".into();
    wire.content = Some(relay_proto::wire_entry::Content::File(
        relay_proto::WireFile {
            object: ObjectId::of(b"pwned").as_bytes().to_vec(),
            size: 5,
            executable: false,
        },
    ));
    h.a.store().put_bytes(b"pwned").unwrap();
    let batch = IndexBatch {
        space_id: space_id_bytes(&space.id),
        entries: vec![wire],
        through_sequence: local.sequence.0 + 10,
        caught_up: true,
        after_sequence: 0,
        plan_files: None,
        plan_bytes: None,
        plan_after: None,
    };
    h.drive(
        SyncInput::Frame {
            peer: h.id_a(),
            body: frame::Body::IndexBatch(batch),
        },
        false,
    );
    assert!(!outside.path().join("x").exists());
    let stored =
        h.b.entries("Personal", "code", true)
            .unwrap()
            .into_iter()
            .any(|e| e.key.path.as_str() == "evil/x");
    assert!(!stored);
}

#[test]
fn large_batch_converges() {
    let mut h = Harness::pair();
    // Two full index batches plus a partial one, without thousands of fsyncs.
    h.sa = Syncer::with_index_batch_entries(100);
    h.sb = Syncer::with_index_batch_entries(100);
    let files: Vec<(String, Vec<u8>)> = (0..250)
        .map(|i| (format!("f{i:04}.txt"), format!("{i}").into_bytes()))
        .collect();
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    h.setup_shared_space(&refs);
    assert_eq!(
        index_triples(&h.a, "Personal", "code").len(),
        index_triples(&h.b, "Personal", "code").len()
    );
    assert_eq!(live_files(h.mount_a.path()), live_files(h.mount_b.path()));
}

#[test]
fn nonempty_dir_tombstone_is_kept() {
    let mut h = Harness::pair();
    h.strict = false;
    h.pair_peers();
    h.a.create_space("Personal").unwrap();
    h.a.add_mount("Personal", "code", h.mount_a.path(), &[], &[])
        .unwrap();
    fs::create_dir_all(h.mount_a.path().join("keep")).unwrap();
    fs::write(h.mount_a.path().join("keep/secret"), b"hidden").unwrap();
    fs::write(h.mount_a.path().join("keep/visible.txt"), b"ok").unwrap();
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.a.share("Personal", "bravo").unwrap();
    h.connect();
    h.pump(VecDeque::new(), None);
    h.b.join_space("Personal", "alpha").unwrap();
    h.b.add_mount(
        "Personal",
        "code",
        h.mount_b.path(),
        &[],
        &["secret".into()],
    )
    .unwrap();
    h.disconnect();
    h.connect();
    h.push_both();
    fs::write(h.mount_b.path().join("keep/secret"), b"excluded-on-b").unwrap();
    fs::remove_dir_all(h.mount_a.path().join("keep")).unwrap();
    h.a.scan(
        "Personal",
        "code",
        ScanOptions {
            allow_mass_delete: true,
            ..ScanOptions::default()
        },
    )
    .unwrap();
    h.push_both();
    assert!(h.mount_b.path().join("keep").is_dir());
    let tombstoned =
        h.b.entries("Personal", "code", true)
            .unwrap()
            .into_iter()
            .any(|e| e.key.path.as_str() == "keep" && e.is_deleted());
    assert!(!tombstoned, "non-empty dir must not store the tombstone");
}

fn entry_at(engine: &Engine, path: &str) -> relay_engine::EntryRecord {
    engine
        .entries("Personal", "code", false)
        .unwrap()
        .into_iter()
        .find(|e| e.key.path.as_str() == path)
        .unwrap_or_else(|| panic!("missing {path}"))
}

#[test]
fn git_metadata_conflicts_share_one_device_winner() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[
        ("repo/.git/HEAD", b"ref: refs/heads/main\n"),
        ("repo/.git/index", b"DIRC-base"),
        ("repo/.git/refs/heads/main", b"base-main\n"),
        ("repo/src/a.txt", b"src"),
    ]);
    h.disconnect();

    let clock_a = Arc::new(ManualClock::new(2_000_000_000_000));
    let clock_b = Arc::new(ManualClock::new(2_000_000_000_000));
    h.a.set_clock(clock_a.clone());
    h.b.set_clock(clock_b.clone());

    let index = LogicalPath::new("repo/.git/index").unwrap();
    let main = LogicalPath::new("repo/.git/refs/heads/main").unwrap();

    // Opposite per-file counters: choose_winner would pick B for index and A
    // for main. The group rule must award both to the greater DeviceId.
    clock_a.set(2_000_000_000_000);
    fs::write(h.mount_a.path().join("repo/.git/index"), b"index-a").unwrap();
    h.a.scan_paths(
        "Personal",
        "code",
        std::slice::from_ref(&index),
        ScanOptions::default(),
    )
    .unwrap();
    clock_a.set(3_000_000_000_000);
    fs::write(
        h.mount_a.path().join("repo/.git/refs/heads/main"),
        b"main-a\n",
    )
    .unwrap();
    h.a.scan_paths(
        "Personal",
        "code",
        std::slice::from_ref(&main),
        ScanOptions::default(),
    )
    .unwrap();

    clock_b.set(2_000_000_000_000);
    fs::write(
        h.mount_b.path().join("repo/.git/refs/heads/main"),
        b"main-b\n",
    )
    .unwrap();
    h.b.scan_paths(
        "Personal",
        "code",
        std::slice::from_ref(&main),
        ScanOptions::default(),
    )
    .unwrap();
    clock_b.set(3_000_000_000_000);
    fs::write(h.mount_b.path().join("repo/.git/index"), b"index-b").unwrap();
    h.b.scan_paths(
        "Personal",
        "code",
        std::slice::from_ref(&index),
        ScanOptions::default(),
    )
    .unwrap();

    let index_a = entry_at(&h.a, "repo/.git/index");
    let index_b = entry_at(&h.b, "repo/.git/index");
    let main_a = entry_at(&h.a, "repo/.git/refs/heads/main");
    let main_b = entry_at(&h.b, "repo/.git/refs/heads/main");
    assert!(
        index_a.vector.get(&h.id_a()) < index_b.vector.get(&h.id_b()),
        "per-file rule would pick B for index"
    );
    assert!(
        main_a.vector.get(&h.id_a()) > main_b.vector.get(&h.id_b()),
        "per-file rule would pick A for main"
    );

    h.connect();
    h.push_both();
    h.push_both();

    let winner_is_a = h.id_a() > h.id_b();
    let (expected_index, expected_main, loser_main) = if winner_is_a {
        (&b"index-a"[..], &b"main-a\n"[..], &main_b)
    } else {
        (&b"index-b"[..], &b"main-b\n"[..], &main_a)
    };
    let copy = conflict_path(
        &main,
        &loser_main.modified_by,
        loser_main.vector.get(&loser_main.modified_by),
    )
    .unwrap();

    let winner = if winner_is_a { h.id_a() } else { h.id_b() };
    for (engine, root) in [(&h.a, h.mount_a.path()), (&h.b, h.mount_b.path())] {
        assert_eq!(
            fs::read(root.join("repo/.git/index")).unwrap(),
            expected_index
        );
        assert_eq!(
            fs::read(root.join("repo/.git/refs/heads/main")).unwrap(),
            expected_main
        );
        assert_eq!(entry_at(engine, "repo/.git/index").modified_by, winner);
        assert_eq!(
            entry_at(engine, "repo/.git/refs/heads/main").modified_by,
            winner
        );
        assert!(
            engine
                .entries("Personal", "code", false)
                .unwrap()
                .iter()
                .any(|e| e.key.path == copy),
            "missing losing ref copy {copy}"
        );
    }

    assert_eq!(
        fs::read(h.mount_a.path().join(copy.as_str())).unwrap(),
        fs::read(h.mount_b.path().join(copy.as_str())).unwrap()
    );
    assert_eq!(
        index_triples(&h.a, "Personal", "code"),
        index_triples(&h.b, "Personal", "code")
    );
    assert_eq!(live_files(h.mount_a.path()), live_files(h.mount_b.path()));
}

fn forty_files() -> Vec<(String, Vec<u8>)> {
    (0..40)
        .map(|i| (format!("f{i:02}.txt"), format!("body-{i}").into_bytes()))
        .collect()
}

fn file_pairs(files: &[(String, Vec<u8>)]) -> Vec<(&str, &[u8])> {
    files
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect()
}

fn delete_first_n(root: &Path, n: usize) {
    for i in 0..n {
        fs::remove_file(root.join(format!("f{i:02}.txt"))).unwrap();
    }
}

fn scan_allow_mass(engine: &mut Engine) {
    engine
        .scan(
            "Personal",
            "code",
            ScanOptions {
                allow_mass_delete: true,
                ..ScanOptions::default()
            },
        )
        .unwrap();
}

#[test]
fn peer_mass_delete_is_held() {
    let files = forty_files();
    let pairs = file_pairs(&files);
    let mut h = Harness::pair();
    h.setup_shared_space(&pairs);
    assert_eq!(live_files(h.mount_a.path()).len(), 40);
    assert_eq!(live_files(h.mount_b.path()).len(), 40);
    let received_before = h.received(false);

    delete_first_n(h.mount_a.path(), 30);
    scan_allow_mass(&mut h.a);
    h.events.clear();
    h.push_both();

    assert_eq!(live_files(h.mount_b.path()).len(), 40);
    let holds = h.b.delete_holds().unwrap();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].deletions, 30);
    assert_eq!(holds[0].live, 40);
    assert!(holds[0].decision.is_none());
    assert!(
        h.events.iter().any(|e| matches!(
            e,
            SyncEvent::DeletesHeld {
                deletions: 30,
                live: 40,
                ..
            }
        )),
        "expected DeletesHeld, got {:?}",
        h.events
    );
    assert_eq!(h.received(false), received_before);
}

#[test]
fn peer_small_delete_is_applied() {
    let files = forty_files();
    let pairs = file_pairs(&files);
    let mut h = Harness::pair();
    h.setup_shared_space(&pairs);
    delete_first_n(h.mount_a.path(), 3);
    scan_allow_mass(&mut h.a);
    h.events.clear();
    h.push_both();

    assert_eq!(live_files(h.mount_b.path()).len(), 37);
    assert!(h.b.delete_holds().unwrap().is_empty());
    assert!(
        !h.events
            .iter()
            .any(|e| matches!(e, SyncEvent::DeletesHeld { .. }))
    );
}

#[test]
fn peer_mass_delete_trips_across_batches() {
    let files = forty_files();
    let pairs = file_pairs(&files);
    let mut h = Harness::pair();
    h.sa = Syncer::with_index_batch_entries(10);
    h.setup_shared_space(&pairs);
    delete_first_n(h.mount_a.path(), 30);
    scan_allow_mass(&mut h.a);
    h.events.clear();
    h.push_both();

    let holds = h.b.delete_holds().unwrap();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].deletions, 30);
    assert_eq!(holds[0].live, 40);
    assert!(
        h.events
            .iter()
            .any(|e| matches!(e, SyncEvent::DeletesHeld { .. })),
        "expected DeletesHeld, got {:?}",
        h.events
    );
    let remaining = live_files(h.mount_b.path()).len();
    assert!(
        remaining > 10,
        "later batches must be held, remaining={remaining}"
    );
}

#[test]
fn apply_held_mass_delete_then_clears_hold() {
    let files = forty_files();
    let pairs = file_pairs(&files);
    let mut h = Harness::pair();
    h.setup_shared_space(&pairs);
    delete_first_n(h.mount_a.path(), 30);
    scan_allow_mass(&mut h.a);
    h.push_both();
    assert_eq!(h.b.delete_holds().unwrap().len(), 1);
    assert_eq!(live_files(h.mount_b.path()).len(), 40);

    let n =
        h.b.decide_delete_hold("Personal", None, None, DeleteHoldDecision::Apply)
            .unwrap();
    assert_eq!(n, 1);
    h.reconnect();
    h.push_both();

    assert_eq!(live_files(h.mount_b.path()).len(), 10);
    assert!(h.b.delete_holds().unwrap().is_empty());
}

#[test]
fn restore_held_mass_delete_converges() {
    let files = forty_files();
    let pairs = file_pairs(&files);
    let mut h = Harness::pair();
    h.setup_shared_space(&pairs);
    delete_first_n(h.mount_a.path(), 30);
    scan_allow_mass(&mut h.a);
    h.push_both();
    assert_eq!(live_files(h.mount_b.path()).len(), 40);

    let n =
        h.b.decide_delete_hold("Personal", None, None, DeleteHoldDecision::Restore)
            .unwrap();
    assert_eq!(n, 1);
    h.reconnect();
    h.push_both();
    h.push_both();

    assert_eq!(live_files(h.mount_b.path()).len(), 40);
    assert_eq!(live_files(h.mount_a.path()).len(), 40);
    assert_eq!(live_files(h.mount_a.path()), live_files(h.mount_b.path()));
    assert_eq!(
        index_triples(&h.a, "Personal", "code"),
        index_triples(&h.b, "Personal", "code")
    );
    assert!(h.b.delete_holds().unwrap().is_empty());
}

#[test]
fn restore_covers_deletes_beyond_the_held_batch() {
    let files: Vec<(String, Vec<u8>)> = (0..100)
        .map(|i| (format!("f{i:02}.txt"), format!("body-{i}").into_bytes()))
        .collect();
    let pairs = file_pairs(&files);
    let mut h = Harness::pair();
    h.sa = Syncer::with_index_batch_entries(10);
    h.setup_shared_space(&pairs);

    delete_first_n(h.mount_a.path(), 80);
    scan_allow_mass(&mut h.a);
    h.push_both();
    assert_eq!(h.b.delete_holds().unwrap().len(), 1);
    let after_hold = live_files(h.mount_b.path());

    for i in 80..90 {
        fs::remove_file(h.mount_a.path().join(format!("f{i:02}.txt"))).unwrap();
    }
    scan_allow_mass(&mut h.a);
    h.b.decide_delete_hold("Personal", None, None, DeleteHoldDecision::Restore)
        .unwrap();
    h.reconnect();
    h.push_both();
    h.push_both();

    let original: BTreeSet<_> = files.into_iter().collect();
    assert_eq!(live_files(h.mount_b.path()), original);
    assert_eq!(live_files(h.mount_a.path()), original);
    assert!(after_hold.iter().any(|(path, _)| path == "f85.txt"));
    assert_eq!(
        index_triples(&h.a, "Personal", "code"),
        index_triples(&h.b, "Personal", "code")
    );
    assert!(h.b.delete_holds().unwrap().is_empty());
}

#[test]
fn restore_keeps_user_recreated_applied_delete() {
    let files: Vec<(String, Vec<u8>)> = (0..100)
        .map(|i| (format!("f{i:02}.txt"), format!("body-{i}").into_bytes()))
        .collect();
    let pairs = file_pairs(&files);
    let mut h = Harness::pair();
    h.sa = Syncer::with_index_batch_entries(10);
    h.setup_shared_space(&pairs);

    delete_first_n(h.mount_a.path(), 80);
    scan_allow_mass(&mut h.a);
    h.push_both();
    assert_eq!(h.b.delete_holds().unwrap().len(), 1);

    let recreated = h.mount_b.path().join("f00.txt");
    assert!(!recreated.exists(), "f00.txt should already be deleted");
    fs::write(&recreated, b"user-kept").unwrap();
    h.b.scan("Personal", "code", ScanOptions::default())
        .unwrap();

    h.b.decide_delete_hold("Personal", None, None, DeleteHoldDecision::Restore)
        .unwrap();
    h.reconnect();
    h.push_both();
    h.push_both();

    assert_eq!(fs::read(&recreated).unwrap(), b"user-kept");
    assert_eq!(
        fs::read(h.mount_a.path().join("f00.txt")).unwrap(),
        b"user-kept"
    );
    let mut expected: BTreeSet<_> = files.into_iter().collect();
    expected.retain(|(path, _)| path != "f00.txt");
    expected.insert(("f00.txt".into(), b"user-kept".to_vec()));
    assert_eq!(live_files(h.mount_a.path()), expected);
    assert_eq!(live_files(h.mount_b.path()), expected);
    assert_eq!(
        index_triples(&h.a, "Personal", "code"),
        index_triples(&h.b, "Personal", "code")
    );
}

#[test]
fn restore_skips_applied_delete_when_object_missing() {
    let files: Vec<(String, Vec<u8>)> = (0..100)
        .map(|i| (format!("f{i:02}.txt"), format!("body-{i}").into_bytes()))
        .collect();
    let pairs = file_pairs(&files);
    let mut h = Harness::pair();
    h.strict = false;
    h.sa = Syncer::with_index_batch_entries(10);
    h.setup_shared_space(&pairs);

    delete_first_n(h.mount_a.path(), 80);
    scan_allow_mass(&mut h.a);
    h.push_both();
    assert_eq!(h.b.delete_holds().unwrap().len(), 1);
    assert!(!h.mount_b.path().join("f00.txt").exists());

    let missing = LogicalPath::new("f00.txt").unwrap();
    let object =
        h.b.history("Personal", "code", &missing)
            .unwrap()
            .into_iter()
            .rev()
            .find_map(|version| match version.content {
                EntryContent::File { object, .. } => Some(object),
                _ => None,
            })
            .expect("prior file version");
    fs::remove_file(h.b.store().path_for(&object)).unwrap();

    h.events.clear();
    h.b.decide_delete_hold("Personal", None, None, DeleteHoldDecision::Restore)
        .unwrap();
    h.reconnect();
    h.push_both();
    h.push_both();

    assert!(
        h.events.iter().any(|e| matches!(
            e,
            SyncEvent::SyncWarning { path, reason, .. }
                if path == "f00.txt" && reason.contains("missing")
        )),
        "expected missing-object warning, got {:?}",
        h.events
    );
    assert!(!h.mount_b.path().join("f00.txt").exists());
    let mut expected: BTreeSet<_> = files.into_iter().collect();
    expected.remove(&("f00.txt".into(), b"body-0".to_vec()));
    assert_eq!(live_files(h.mount_b.path()), expected);
    assert_eq!(live_files(h.mount_a.path()), expected);
    assert_eq!(
        index_triples(&h.a, "Personal", "code"),
        index_triples(&h.b, "Personal", "code")
    );
}

#[test]
fn hold_on_one_space_does_not_block_another() {
    let files = forty_files();
    let pairs = file_pairs(&files);
    let mut h = Harness::pair();
    h.setup_shared_space(&pairs);

    let work_a = tempfile::TempDir::new().unwrap();
    let work_b = tempfile::TempDir::new().unwrap();
    h.a.create_space("Work").unwrap();
    h.a.add_mount("Work", "docs", work_a.path(), &[], &[])
        .unwrap();
    write_tree(work_a.path(), &[("note.txt", b"v1")]);
    h.a.scan("Work", "docs", ScanOptions::default()).unwrap();
    h.a.share("Work", "bravo").unwrap();
    h.disconnect();
    h.connect();
    h.b.join_space("Work", "alpha").unwrap();
    h.b.add_mount("Work", "docs", work_b.path(), &[], &[])
        .unwrap();
    h.disconnect();
    h.connect();
    h.push_both();
    assert_eq!(fs::read(work_b.path().join("note.txt")).unwrap(), b"v1");

    delete_first_n(h.mount_a.path(), 30);
    scan_allow_mass(&mut h.a);
    fs::write(work_a.path().join("note.txt"), b"v2").unwrap();
    h.a.scan("Work", "docs", ScanOptions::default()).unwrap();
    h.push_both();

    assert_eq!(h.b.delete_holds().unwrap().len(), 1);
    assert_eq!(live_files(h.mount_b.path()).len(), 40);
    assert_eq!(fs::read(work_b.path().join("note.txt")).unwrap(), b"v2");
}

#[test]
fn mount_marker_is_not_indexed_or_replicated() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("hello.txt", b"hi")]);

    for (label, engine) in [("a", &h.a), ("b", &h.b)] {
        let paths: Vec<_> = engine
            .entries("Personal", "code", true)
            .unwrap()
            .into_iter()
            .map(|e| e.key.path.to_string())
            .collect();
        assert!(
            !paths
                .iter()
                .any(|p| p == MOUNT_MARKER || p.ends_with(&format!("/{MOUNT_MARKER}"))),
            "{label} indexed the mount marker: {paths:?}"
        );
        assert!(paths.iter().any(|p| p == "hello.txt"), "{label}: {paths:?}");
    }

    let marker_a = MountMarker::read(h.mount_a.path()).unwrap();
    let marker_b = MountMarker::read(h.mount_b.path()).unwrap();
    assert_eq!(marker_a.space, marker_b.space);
    assert_eq!(marker_a.mount, marker_b.mount);
    // Each device writes its own local bookkeeping file; it is not synced.
    assert_ne!(marker_a.created_by, marker_b.created_by);
    assert_eq!(marker_a.created_by, h.id_a());
    assert_eq!(marker_b.created_by, h.id_b());

    // A hostile peer that forges a marker entry must not overwrite local bookkeeping.
    h.strict = false;
    let before = fs::read(h.mount_b.path().join(MOUNT_MARKER)).unwrap();
    let space = h.a.spaces().unwrap().into_iter().next().unwrap();
    let local =
        h.a.entries("Personal", "code", false)
            .unwrap()
            .into_iter()
            .find(|e| e.key.path.as_str() == "hello.txt")
            .unwrap();
    let mut wire = entry_to_wire(&local);
    wire.path = MOUNT_MARKER.into();
    let forged = b"forged-marker-as-user-content";
    h.a.store().put_bytes(forged).unwrap();
    wire.content = Some(relay_proto::wire_entry::Content::File(
        relay_proto::WireFile {
            object: ObjectId::of(forged).as_bytes().to_vec(),
            size: forged.len() as u64,
            executable: false,
        },
    ));
    let batch = IndexBatch {
        space_id: space_id_bytes(&space.id),
        entries: vec![wire],
        through_sequence: local.sequence.0 + 10,
        caught_up: true,
        after_sequence: 0,
        plan_files: None,
        plan_bytes: None,
        plan_after: None,
    };
    h.drive(
        SyncInput::Frame {
            peer: h.id_a(),
            body: frame::Body::IndexBatch(batch),
        },
        false,
    );
    assert_eq!(
        fs::read(h.mount_b.path().join(MOUNT_MARKER)).unwrap(),
        before
    );
    assert_eq!(
        MountMarker::read(h.mount_b.path()).unwrap().created_by,
        h.id_b()
    );
    assert!(
        !h.b.entries("Personal", "code", true)
            .unwrap()
            .iter()
            .any(|e| e.key.path.as_str() == MOUNT_MARKER)
    );
}

/// Three engines with A–B and B–C links only. After each handle, push that
/// engine's local changes (same as the watch loop) so applied remote entries
/// are offered onward.
struct Hub {
    _homes: [TempDir; 3],
    mounts: [TempDir; 3],
    engines: [Engine; 3],
    syncers: [Syncer; 3],
    strict: bool,
    events: Vec<SyncEvent>,
    /// Conflict resolutions each engine performed, by hub index.
    conflicts_by_engine: [usize; 3],
}

impl Hub {
    fn new() -> Self {
        let homes = [
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
        ];
        let mounts = [
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
        ];
        let engines = [
            init(homes[0].path(), "alpha", std::time::Duration::ZERO),
            init(homes[1].path(), "bravo", std::time::Duration::ZERO),
            init(homes[2].path(), "charlie", std::time::Duration::ZERO),
        ];
        Self {
            _homes: homes,
            mounts,
            engines,
            syncers: [Syncer::new(), Syncer::new(), Syncer::new()],
            strict: true,
            events: Vec::new(),
            conflicts_by_engine: [0; 3],
        }
    }

    fn record(&mut self, engine: usize, events: Vec<SyncEvent>) {
        assert_no_warnings(&events, self.strict);
        self.conflicts_by_engine[engine] += events
            .iter()
            .map(|e| match e {
                SyncEvent::RemoteApplied { conflicts, .. } => *conflicts,
                _ => 0,
            })
            .sum::<usize>();
        self.events.extend(events);
    }

    fn id(&self, i: usize) -> DeviceId {
        self.engines[i].device().id
    }

    fn name(i: usize) -> &'static str {
        ["alpha", "bravo", "charlie"][i]
    }

    fn index_of(&self, id: DeviceId) -> usize {
        (0..3)
            .find(|&i| self.id(i) == id)
            .expect("unknown device in hub pump")
    }

    fn connect_pair(&mut self, left: usize, right: usize) {
        let mut q = VecDeque::new();
        q.push_back((
            left,
            SyncInput::PeerConnected {
                peer: self.id(right),
                name: Self::name(right).into(),
            },
        ));
        q.push_back((
            right,
            SyncInput::PeerConnected {
                peer: self.id(left),
                name: Self::name(left).into(),
            },
        ));
        self.pump(q);
    }

    fn push_all(&mut self) {
        let mut outputs = VecDeque::new();
        for i in 0..3 {
            let mut outs = Vec::new();
            let events = self.syncers[i]
                .push_local_changes(&mut self.engines[i], &mut |o| outs.push(o))
                .unwrap();
            self.record(i, events);
            for o in outs {
                outputs.push_back((i, o));
            }
        }
        self.drain(VecDeque::new(), outputs);
    }

    fn pump(&mut self, q: VecDeque<(usize, SyncInput)>) {
        self.drain(q, VecDeque::new());
    }

    fn drain(
        &mut self,
        mut q: VecDeque<(usize, SyncInput)>,
        mut outputs: VecDeque<(usize, SyncOutput)>,
    ) {
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(steps < 50_000, "hub pump did not go quiet");
            if let Some((to, input)) = q.pop_front() {
                let mut outs = Vec::new();
                let events = self.syncers[to]
                    .handle(&mut self.engines[to], input, &mut |o| outs.push(o))
                    .unwrap();
                self.record(to, events);
                // Mirror the watch loop: after applying, offer local sequences onward.
                let mut pushed = Vec::new();
                let push_events = self.syncers[to]
                    .push_local_changes(&mut self.engines[to], &mut |o| pushed.push(o))
                    .unwrap();
                self.record(to, push_events);
                for o in outs.into_iter().chain(pushed) {
                    outputs.push_back((to, o));
                }
                continue;
            }
            let Some((from, output)) = outputs.pop_front() else {
                break;
            };
            match output {
                SyncOutput::Send { peer, body } => {
                    let to = self.index_of(peer);
                    assert_ne!(to, from, "send went to the sender");
                    q.push_back((
                        to,
                        SyncInput::Frame {
                            peer: self.id(from),
                            body,
                        },
                    ));
                }
                SyncOutput::SetPeers | SyncOutput::SetRelay(_) => {}
                SyncOutput::FetchObject { peer, object } => {
                    let src = self.index_of(peer);
                    let input = copy_object(&self.engines[src], &self.engines[from], peer, object);
                    q.push_back((from, input));
                }
            }
        }
    }
}

/// A store-mode hub (the home server, D47) keeps no working-tree files but
/// still serves every object to a device the writer never meets.
#[test]
fn store_hub_serves_devices_that_never_meet_the_writer() {
    let mut h = Hub::new();
    h.engines[0]
        .add_peer("bravo", h.id(1), &["127.0.0.1:47321".into()])
        .unwrap();
    h.engines[1]
        .add_peer("alpha", h.id(0), &["127.0.0.1:47321".into()])
        .unwrap();
    h.engines[1]
        .add_peer("charlie", h.id(2), &["127.0.0.1:47321".into()])
        .unwrap();
    h.engines[2]
        .add_peer("bravo", h.id(1), &["127.0.0.1:47321".into()])
        .unwrap();

    h.engines[0].create_space("Personal").unwrap();
    h.engines[0]
        .add_mount("Personal", "code", h.mounts[0].path(), &[], &[])
        .unwrap();
    write_tree(h.mounts[0].path(), &[("seed.txt", b"seed")]);
    h.engines[0]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.engines[0].share("Personal", "bravo").unwrap();

    h.connect_pair(0, 1);
    h.engines[1].join_space("Personal", "alpha").unwrap();
    h.engines[1]
        .set_folder_mode("Personal", "code", "", Some(MaterializationMode::Store))
        .unwrap();
    h.engines[1]
        .add_mount("Personal", "code", h.mounts[1].path(), &[], &[])
        .unwrap();
    h.engines[1].share("Personal", "charlie").unwrap();
    h.syncers[0] = Syncer::new();
    h.syncers[1] = Syncer::new();
    h.connect_pair(0, 1);
    h.push_all();

    h.connect_pair(1, 2);
    h.engines[2].join_space("Personal", "bravo").unwrap();
    h.engines[2]
        .add_mount("Personal", "code", h.mounts[2].path(), &[], &[])
        .unwrap();
    h.syncers[1] = Syncer::new();
    h.syncers[2] = Syncer::new();
    h.connect_pair(1, 2);
    h.connect_pair(0, 1);
    h.push_all();

    assert_eq!(
        fs::read(h.mounts[2].path().join("seed.txt")).unwrap(),
        b"seed"
    );
    assert!(live_files(h.mounts[1].path()).is_empty());

    fs::write(h.mounts[0].path().join("from-a.txt"), b"a-side").unwrap();
    h.engines[0]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_all();
    assert_eq!(
        fs::read(h.mounts[2].path().join("from-a.txt")).unwrap(),
        b"a-side"
    );
    assert!(live_files(h.mounts[1].path()).is_empty());
    assert!(h.engines[1].store().contains(&ObjectId::of(b"a-side")));
}

#[test]
fn hub_forwards_between_devices_that_are_not_directly_connected() {
    let mut h = Hub::new();
    // A↔B and B↔C only; never A↔C.
    h.engines[0]
        .add_peer("bravo", h.id(1), &["127.0.0.1:47321".into()])
        .unwrap();
    h.engines[1]
        .add_peer("alpha", h.id(0), &["127.0.0.1:47321".into()])
        .unwrap();
    h.engines[1]
        .add_peer("charlie", h.id(2), &["127.0.0.1:47321".into()])
        .unwrap();
    h.engines[2]
        .add_peer("bravo", h.id(1), &["127.0.0.1:47321".into()])
        .unwrap();

    h.engines[0].create_space("Personal").unwrap();
    h.engines[0]
        .add_mount("Personal", "code", h.mounts[0].path(), &[], &[])
        .unwrap();
    write_tree(h.mounts[0].path(), &[("seed.txt", b"seed")]);
    h.engines[0]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.engines[0].share("Personal", "bravo").unwrap();

    h.connect_pair(0, 1);
    h.engines[1].join_space("Personal", "alpha").unwrap();
    h.engines[1]
        .add_mount("Personal", "code", h.mounts[1].path(), &[], &[])
        .unwrap();
    h.engines[1].share("Personal", "charlie").unwrap();

    // Reconnect A–B so B's mount is in offers; then connect B–C.
    h.syncers[0] = Syncer::new();
    h.syncers[1] = Syncer::new();
    h.connect_pair(0, 1);
    h.push_all();

    h.connect_pair(1, 2);
    h.engines[2].join_space("Personal", "bravo").unwrap();
    h.engines[2]
        .add_mount("Personal", "code", h.mounts[2].path(), &[], &[])
        .unwrap();
    h.syncers[1] = Syncer::new();
    h.syncers[2] = Syncer::new();
    h.connect_pair(1, 2);
    // Keep A–B connected too.
    h.connect_pair(0, 1);
    h.push_all();

    assert_eq!(
        live_files(h.mounts[0].path()),
        live_files(h.mounts[2].path()),
        "initial seed should reach C through B"
    );

    fs::write(h.mounts[0].path().join("from-a.txt"), b"a-side").unwrap();
    h.engines[0]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_all();
    assert_eq!(
        fs::read(h.mounts[2].path().join("from-a.txt")).unwrap(),
        b"a-side"
    );

    fs::write(h.mounts[2].path().join("from-c.txt"), b"c-side").unwrap();
    h.engines[2]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_all();
    assert_eq!(
        fs::read(h.mounts[0].path().join("from-c.txt")).unwrap(),
        b"c-side"
    );
}

#[test]
fn joined_space_trusts_members_listed_on_the_offer() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("hello.txt", b"hi")]);

    let home_c = TempDir::new().unwrap();
    let c = init(home_c.path(), "charlie", std::time::Duration::ZERO);
    let id_c = c.device().id;

    h.b.add_peer("charlie", id_c, &["127.0.0.1:47322".into()])
        .unwrap();
    h.b.share("Personal", "charlie").unwrap();

    let offers = h.b.space_offers_for_peer(h.id_a()).unwrap();
    assert_eq!(offers.spaces.len(), 1);
    assert_eq!(offers.spaces[0].members.len(), 1);
    assert_eq!(
        offers.spaces[0].members[0].device_id,
        id_c.as_bytes().to_vec()
    );
    assert_eq!(offers.spaces[0].members[0].name, "charlie");

    let peer_b = h.id_b();
    let mut outs = Vec::new();
    let events =
        h.sa.handle(
            &mut h.a,
            SyncInput::Frame {
                peer: peer_b,
                body: frame::Body::SpaceOffers(offers),
            },
            &mut |o| outs.push(o),
        )
        .unwrap();
    assert_no_warnings(&events, true);
    assert!(
        outs.iter().any(|o| matches!(o, SyncOutput::SetPeers)),
        "adopting a member should emit SetPeers: {outs:?}"
    );
    assert!(
        h.a.peers().unwrap().iter().any(|p| p.id == id_c),
        "A should trust C after offer from a joined space"
    );
    assert!(
        h.a.status()
            .unwrap()
            .peers
            .iter()
            .find(|p| p.name == "charlie")
            .is_some_and(|p| p.spaces.iter().any(|s| s.space == "Personal")),
        "A should share Personal with C"
    );

    // Offer for a space A has not joined must not introduce members.
    let foreign = relay_proto::SpaceOffers {
        spaces: vec![relay_proto::SpaceOffer {
            space_id: space_id_bytes(&SpaceId::new()),
            name: "Strangers".into(),
            mounts: vec![],
            members: vec![relay_proto::MemberOffer {
                device_id: DeviceId::from_bytes([9; 32]).as_bytes().to_vec(),
                name: "nobody".into(),
                addresses: vec!["127.0.0.1:9".into()],
            }],
            policy_epoch: 0,
            policies: vec![],
        }],
    };
    let before = h.a.peers().unwrap().len();
    let mut outs = Vec::new();
    let events =
        h.sa.handle(
            &mut h.a,
            SyncInput::Frame {
                peer: peer_b,
                body: frame::Body::SpaceOffers(foreign),
            },
            &mut |o| outs.push(o),
        )
        .unwrap();
    assert_no_warnings(&events, true);
    assert!(!outs.iter().any(|o| matches!(o, SyncOutput::SetPeers)));
    assert_eq!(h.a.peers().unwrap().len(), before);
    assert!(
        h.a.peers()
            .unwrap()
            .iter()
            .all(|p| p.id != DeviceId::from_bytes([9; 32]))
    );
}

#[test]
fn join_adopts_offered_members() {
    let mut h = Harness::pair();
    h.pair_peers();
    h.a.create_space("Personal").unwrap();
    h.a.add_mount("Personal", "code", h.mount_a.path(), &[], &[])
        .unwrap();
    h.a.share("Personal", "bravo").unwrap();

    let id_c = DeviceId::from_bytes([0xcc; 32]);
    let space = h.a.spaces().unwrap().into_iter().next().unwrap();
    let mount =
        h.a.mounts(Some("Personal"))
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .1
            .mount;
    let offers = relay_proto::SpaceOffers {
        spaces: vec![relay_proto::SpaceOffer {
            space_id: space_id_bytes(&space.id),
            name: space.name,
            mounts: vec![relay_proto::MountOffer {
                mount_id: mount_id_bytes(&mount.id),
                name: mount.name,
            }],
            members: vec![relay_proto::MemberOffer {
                device_id: id_c.as_bytes().to_vec(),
                name: "charlie".into(),
                addresses: vec!["127.0.0.1:47322".into()],
            }],
            policy_epoch: 0,
            policies: vec![],
        }],
    };
    // B has not joined yet, so members are stored but not adopted.
    let peer_a = h.id_a();
    h.sb.handle(
        &mut h.b,
        SyncInput::PeerConnected {
            peer: peer_a,
            name: "alpha".into(),
        },
        &mut |_| {},
    )
    .unwrap();
    h.sb.handle(
        &mut h.b,
        SyncInput::Frame {
            peer: peer_a,
            body: frame::Body::SpaceOffers(offers),
        },
        &mut |_| {},
    )
    .unwrap();
    assert!(
        h.b.peers().unwrap().iter().all(|p| p.id != id_c),
        "members on an unjoined offer must not be trusted yet"
    );

    h.b.join_space("Personal", "alpha").unwrap();
    assert!(
        h.b.peers().unwrap().iter().any(|p| p.id == id_c),
        "join should adopt offered member charlie"
    );
    let shared =
        h.b.status()
            .unwrap()
            .peers
            .iter()
            .find(|p| p.name == "charlie")
            .expect("charlie peer")
            .spaces
            .iter()
            .any(|s| s.space == "Personal");
    assert!(shared, "join should share the space with adopted members");
}

#[test]
fn joined_space_drops_out_of_offers_until_left() {
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let mount_a = TempDir::new().unwrap();
    let mut a = init(home_a.path(), "alpha", std::time::Duration::ZERO);
    let mut b = init(home_b.path(), "bravo", std::time::Duration::ZERO);
    a.add_peer("bravo", b.device().id, &["127.0.0.1:47321".into()])
        .unwrap();
    b.add_peer("alpha", a.device().id, &["127.0.0.1:47321".into()])
        .unwrap();
    a.create_space("Personal").unwrap();
    a.add_mount("Personal", "code", mount_a.path(), &[], &[])
        .unwrap();
    a.share("Personal", "bravo").unwrap();

    let wire = a.space_offers_for_peer(b.device().id).unwrap();
    let peer_a = a.device().id;
    let mut sb = Syncer::new();
    sb.handle(
        &mut b,
        SyncInput::PeerConnected {
            peer: peer_a,
            name: "alpha".into(),
        },
        &mut |_| {},
    )
    .unwrap();
    sb.handle(
        &mut b,
        SyncInput::Frame {
            peer: peer_a,
            body: frame::Body::SpaceOffers(wire),
        },
        &mut |_| {},
    )
    .unwrap();

    assert!(
        b.offers().unwrap().iter().any(|o| o.name == "Personal"),
        "an unjoined offer is joinable"
    );
    b.join_space("Personal", "alpha").unwrap();
    assert!(
        b.offers().unwrap().iter().all(|o| o.name != "Personal"),
        "joining hides the offer"
    );

    // Leave: the offer row stays, so removing the local space lists it again.
    drop(b);
    let db = rusqlite::Connection::open(home_b.path().join("relay.db")).unwrap();
    db.execute_batch(
        "PRAGMA foreign_keys = ON;
         DELETE FROM space_shares;
         DELETE FROM sync_progress;
         DELETE FROM mounts;
         DELETE FROM spaces;",
    )
    .unwrap();
    drop(db);

    let b = Engine::open_read_only(home_b.path()).unwrap();
    assert!(
        b.offers().unwrap().iter().any(|o| o.name == "Personal"),
        "leaving brings the offer back"
    );
}

#[test]
fn removed_peer_is_not_reintroduced() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("hello.txt", b"hi")]);
    let id_c = DeviceId::from_bytes([0xcc; 32]);
    let members = [relay_db::OfferedMember {
        id: id_c,
        name: "charlie".into(),
        addresses: vec!["127.0.0.1:47322".into()],
    }];
    let space = h.a.spaces().unwrap()[0].id;
    h.a.adopt_offered_members(space, &members).unwrap();
    assert!(h.a.peers().unwrap().iter().any(|p| p.id == id_c));

    h.a.remove_peer("charlie").unwrap();
    assert!(h.a.peers().unwrap().iter().all(|p| p.id != id_c));

    let again = h.a.adopt_offered_members(space, &members).unwrap();
    assert!(!again.peers_changed);
    assert!(again.newly_shared.is_empty());
    assert!(h.a.peers().unwrap().iter().all(|p| p.id != id_c));

    // Explicit add clears dismissal.
    h.a.add_peer("charlie", id_c, &["127.0.0.1:47322".into()])
        .unwrap();
    assert!(h.a.peers().unwrap().iter().any(|p| p.id == id_c));
    h.a.remove_peer("charlie").unwrap();

    // upsert_peer of the same id also clears dismissal.
    h.a.upsert_peer("charlie", id_c, &["127.0.0.1:47322".into()])
        .unwrap();
    assert!(h.a.peers().unwrap().iter().any(|p| p.id == id_c));

    // After remove, a later offer still skips until cleared again.
    h.a.remove_peer("charlie").unwrap();
    let mut outs = Vec::new();
    let space_rec = h.a.spaces().unwrap().into_iter().next().unwrap();
    let peer_b = h.id_b();
    let offer = relay_proto::SpaceOffers {
        spaces: vec![relay_proto::SpaceOffer {
            space_id: space_id_bytes(&space_rec.id),
            name: space_rec.name,
            mounts: vec![],
            members: vec![relay_proto::MemberOffer {
                device_id: id_c.as_bytes().to_vec(),
                name: "charlie".into(),
                addresses: vec!["127.0.0.1:47322".into()],
            }],
            policy_epoch: 0,
            policies: vec![],
        }],
    };
    h.sa.handle(
        &mut h.a,
        SyncInput::Frame {
            peer: peer_b,
            body: frame::Body::SpaceOffers(offer),
        },
        &mut |o| outs.push(o),
    )
    .unwrap();
    assert!(h.a.peers().unwrap().iter().all(|p| p.id != id_c));
}

fn hub_fully_mesh(h: &mut Hub) {
    for i in 0..3 {
        for j in 0..3 {
            if i == j {
                continue;
            }
            let name = Hub::name(j);
            let id = h.id(j);
            if h.engines[i].peers().unwrap().iter().all(|p| p.id != id) {
                h.engines[i]
                    .add_peer(name, id, &["127.0.0.1:47321".into()])
                    .unwrap();
            }
        }
    }
}

fn hub_reconnect_all(h: &mut Hub) {
    for i in 0..3 {
        h.syncers[i] = Syncer::new();
    }
    h.connect_pair(0, 1);
    h.connect_pair(1, 2);
    h.connect_pair(0, 2);
    h.push_all();
}

#[test]
fn replication_policies_partition_subtrees_across_three_devices() {
    let mut h = Hub::new();
    hub_fully_mesh(&mut h);

    h.engines[0].create_space("Personal").unwrap();
    h.engines[0]
        .add_mount("Personal", "code", h.mounts[0].path(), &[], &[])
        .unwrap();
    // No policies yet: a seed file still syncs to every shared peer.
    write_tree(h.mounts[0].path(), &[("seed.txt", b"seed")]);
    h.engines[0]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.engines[0].share("Personal", "bravo").unwrap();
    h.engines[0].share("Personal", "charlie").unwrap();

    h.connect_pair(0, 1);
    h.engines[1].join_space("Personal", "alpha").unwrap();
    h.engines[1]
        .add_mount("Personal", "code", h.mounts[1].path(), &[], &[])
        .unwrap();
    h.engines[1].share("Personal", "charlie").unwrap();

    h.connect_pair(0, 2);
    h.engines[2].join_space("Personal", "alpha").unwrap();
    h.engines[2]
        .add_mount("Personal", "code", h.mounts[2].path(), &[], &[])
        .unwrap();
    hub_reconnect_all(&mut h);

    assert!(h.mounts[1].path().join("seed.txt").is_file());
    assert!(h.mounts[2].path().join("seed.txt").is_file());

    h.engines[0]
        .policy_add(
            "Personal",
            "personal",
            &["code/personal/**".into()],
            &["alpha".into(), "bravo".into()],
            &[],
        )
        .unwrap();
    h.engines[0]
        .policy_add(
            "Personal",
            "work",
            &["code/work/**".into()],
            &["bravo".into(), "charlie".into()],
            &[],
        )
        .unwrap();
    hub_reconnect_all(&mut h);

    write_tree(h.mounts[0].path(), &[("personal/a.txt", b"personal-a")]);
    h.engines[0]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    write_tree(h.mounts[2].path(), &[("work/b.txt", b"work-b")]);
    h.engines[2]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_all();

    assert_eq!(
        fs::read(h.mounts[0].path().join("personal/a.txt")).unwrap(),
        b"personal-a"
    );
    assert_eq!(
        fs::read(h.mounts[1].path().join("personal/a.txt")).unwrap(),
        b"personal-a"
    );
    assert!(
        !h.mounts[2].path().join("personal/a.txt").exists(),
        "C must not receive personal/"
    );
    assert!(
        !h.mounts[0].path().join("work/b.txt").exists(),
        "A must not receive work/"
    );
    assert_eq!(
        fs::read(h.mounts[1].path().join("work/b.txt")).unwrap(),
        b"work-b"
    );
    assert_eq!(
        fs::read(h.mounts[2].path().join("work/b.txt")).unwrap(),
        b"work-b"
    );
    assert!(
        !h.mounts[0]
            .path()
            .join("personal")
            .read_dir()
            .into_iter()
            .flatten()
            .any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("relay-conflict")),
        "no conflict copies on A"
    );

    fs::write(h.mounts[0].path().join("personal/a.txt"), b"edited").unwrap();
    h.engines[0]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_all();
    assert_eq!(
        fs::read(h.mounts[1].path().join("personal/a.txt")).unwrap(),
        b"edited"
    );
    assert!(!h.mounts[2].path().join("personal/a.txt").exists());
}

#[test]
fn removing_a_policy_preserves_existing_files() {
    let mut h = Harness::pair();
    h.pair_peers();
    h.a.create_space("Personal").unwrap();
    h.a.add_mount("Personal", "code", h.mount_a.path(), &[], &[])
        .unwrap();
    h.a.share("Personal", "bravo").unwrap();
    h.connect();
    h.b.join_space("Personal", "alpha").unwrap();
    h.b.add_mount("Personal", "code", h.mount_b.path(), &[], &[])
        .unwrap();
    h.reconnect();

    h.a.policy_add(
        "Personal",
        "all",
        &["code/**".into()],
        &["alpha".into(), "bravo".into()],
        &[],
    )
    .unwrap();
    h.reconnect();

    write_tree(h.mount_a.path(), &[("keep.txt", b"keep-me")]);
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    assert_eq!(
        fs::read(h.mount_b.path().join("keep.txt")).unwrap(),
        b"keep-me"
    );

    h.a.policy_remove("Personal", "all").unwrap();
    h.reconnect();
    h.push_both();

    assert_eq!(
        fs::read(h.mount_a.path().join("keep.txt")).unwrap(),
        b"keep-me"
    );
    assert_eq!(
        fs::read(h.mount_b.path().join("keep.txt")).unwrap(),
        b"keep-me"
    );
    let deleted =
        h.b.entries("Personal", "code", true)
            .unwrap()
            .into_iter()
            .any(|e| e.key.path.as_str() == "keep.txt" && e.is_deleted());
    assert!(!deleted, "removal must not tombstone keep.txt");
}

#[test]
fn device_group_expansion_widens_targets_after_epoch_exchange() {
    let mut h = Hub::new();
    hub_fully_mesh(&mut h);

    h.engines[0].create_space("Personal").unwrap();
    h.engines[0]
        .add_mount("Personal", "code", h.mounts[0].path(), &[], &[])
        .unwrap();
    h.engines[0].share("Personal", "bravo").unwrap();
    h.engines[0].share("Personal", "charlie").unwrap();

    h.connect_pair(0, 1);
    h.engines[1].join_space("Personal", "alpha").unwrap();
    h.engines[1]
        .add_mount("Personal", "code", h.mounts[1].path(), &[], &[])
        .unwrap();

    h.connect_pair(0, 2);
    h.engines[2].join_space("Personal", "alpha").unwrap();
    h.engines[2]
        .add_mount("Personal", "code", h.mounts[2].path(), &[], &[])
        .unwrap();
    hub_reconnect_all(&mut h);

    h.engines[0].group_create("lan").unwrap();
    h.engines[0].group_add("lan", "bravo").unwrap();
    h.engines[0]
        .policy_add(
            "Personal",
            "shared",
            &["code/shared/**".into()],
            &["alpha".into()],
            &["lan".into()],
        )
        .unwrap();
    hub_reconnect_all(&mut h);

    write_tree(h.mounts[0].path(), &[("shared/one.txt", b"one")]);
    h.engines[0]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_all();
    assert_eq!(
        fs::read(h.mounts[1].path().join("shared/one.txt")).unwrap(),
        b"one"
    );
    assert!(!h.mounts[2].path().join("shared/one.txt").exists());

    h.engines[0].group_add("lan", "charlie").unwrap();
    hub_reconnect_all(&mut h);
    write_tree(h.mounts[0].path(), &[("shared/two.txt", b"two")]);
    h.engines[0]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_all();
    assert_eq!(
        fs::read(h.mounts[2].path().join("shared/two.txt")).unwrap(),
        b"two"
    );
}

#[test]
fn disconnect_records_when_a_peer_went_offline() {
    let mut h = Harness::pair();
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    h.a.set_clock(Arc::clone(&clock) as Arc<dyn Clock>);
    h.pair_peers();
    assert_eq!(h.a.peers().unwrap()[0].last_seen_ms, None);

    h.connect();
    assert_eq!(
        h.a.peers().unwrap()[0].last_seen_ms,
        Some(1_700_000_000_000),
        "connecting counts as a sighting"
    );

    clock.set(1_700_000_000_000 + 45_000);
    h.sa.tick(
        &mut h.a,
        std::time::Instant::now() + std::time::Duration::from_secs(31),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        h.a.peers().unwrap()[0].last_seen_ms,
        Some(1_700_000_000_000 + 45_000),
        "a live session keeps the last-seen stamp fresh"
    );

    clock.set(1_700_000_000_000 + 3_600_000);
    h.disconnect();
    assert_eq!(
        h.a.peers().unwrap()[0].last_seen_ms,
        Some(1_700_000_000_000 + 3_600_000),
        "offline duration starts when the session ends"
    );
}

fn publish(h: &mut Harness, files: &[(&str, &[u8])]) {
    write_tree(h.mount_a.path(), files);
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.fetches.clear();
    h.push_both();
}

#[test]
fn file_syncs_to_disk_without_materialization_rules() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("note.txt", b"hello")]);
    assert_eq!(
        fs::read(h.mount_b.path().join("note.txt")).unwrap(),
        b"hello"
    );
    assert!(entry_at(&h.b, "note.txt").materialized);
}

#[test]
fn metadata_rule_keeps_the_index_and_does_not_fetch_or_tombstone() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[]);
    h.b.materialize_add("Personal", "notes", "metadata", &["code/note.txt".into()])
        .unwrap();
    publish(&mut h, &[("note.txt", b"hello")]);

    let entry = entry_at(&h.b, "note.txt");
    assert!(!entry.materialized);
    assert_eq!(entry.content.object().unwrap(), ObjectId::of(b"hello"));
    assert!(!h.mount_b.path().join("note.txt").exists());
    assert!(
        !h.fetches.contains(&ObjectId::of(b"hello")),
        "metadata must not fetch the object"
    );

    h.b.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    assert!(!entry_at(&h.b, "note.txt").is_deleted());
    assert_eq!(
        fs::read(h.mount_a.path().join("note.txt")).unwrap(),
        b"hello"
    );
}

#[test]
fn store_rule_fetches_bytes_without_writing_or_tombstoning() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[]);
    h.b.materialize_add("Personal", "server", "store", &["code/**".into()])
        .unwrap();
    publish(&mut h, &[("note.txt", b"hello")]);

    let entry = entry_at(&h.b, "note.txt");
    assert!(!entry.materialized);
    assert!(!h.mount_b.path().join("note.txt").exists());
    assert!(h.fetches.contains(&ObjectId::of(b"hello")));
    assert!(h.b.store().contains(&ObjectId::of(b"hello")));
    assert!(h.b.verify_objects().unwrap().missing.is_empty());

    h.b.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    assert!(!entry_at(&h.b, "note.txt").is_deleted());
    assert_eq!(
        fs::read(h.mount_a.path().join("note.txt")).unwrap(),
        b"hello"
    );

    // A later edit replaces the stored bytes; a delete reaches the index.
    publish(
        &mut h,
        &[("note.txt", b"hello again"), ("keep.txt", b"keep")],
    );
    assert!(h.b.store().contains(&ObjectId::of(b"hello again")));
    assert!(!h.mount_b.path().join("note.txt").exists());
    fs::remove_file(h.mount_a.path().join("note.txt")).unwrap();
    publish(&mut h, &[]);
    assert!(
        h.b.entries("Personal", "code", true)
            .unwrap()
            .iter()
            .any(|e| e.key.path.as_str() == "note.txt" && e.is_deleted())
    );
}

/// Files written while a path was `full` stay current after it goes to
/// `store`, so switching back never finds stale bytes to version.
#[test]
fn store_keeps_files_written_before_the_switch_current() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("note.txt", b"one"), ("keep.txt", b"keep")]);
    h.b.set_folder_mode("Personal", "code", "", Some(MaterializationMode::Store))
        .unwrap();
    publish(&mut h, &[("note.txt", b"two")]);
    assert_eq!(fs::read(h.mount_b.path().join("note.txt")).unwrap(), b"two");
    assert!(entry_at(&h.b, "note.txt").materialized);

    // A new file stays in the store only.
    publish(&mut h, &[("new.txt", b"new")]);
    assert!(!h.mount_b.path().join("new.txt").exists());

    h.b.set_folder_mode("Personal", "code", "", None).unwrap();
    h.tick_b(std::time::Instant::now());
    h.b.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    assert_eq!(fs::read(h.mount_a.path().join("note.txt")).unwrap(), b"two");
    assert_eq!(fs::read(h.mount_b.path().join("new.txt")).unwrap(), b"new");
    assert_converged(&h);
}

#[test]
fn switching_metadata_to_store_fetches_on_tick_without_writing() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[]);
    h.b.materialize_add("Personal", "notes", "metadata", &["code/**".into()])
        .unwrap();
    publish(&mut h, &[("note.txt", b"hello")]);
    assert!(!h.b.store().contains(&ObjectId::of(b"hello")));

    h.b.set_folder_mode("Personal", "code", "", Some(MaterializationMode::Store))
        .unwrap();
    h.b.materialize_remove("Personal", "notes").unwrap();
    h.fetches.clear();
    h.tick_b(std::time::Instant::now());
    assert!(h.fetches.contains(&ObjectId::of(b"hello")));
    assert!(h.b.store().contains(&ObjectId::of(b"hello")));
    assert!(!h.mount_b.path().join("note.txt").exists());
    assert!(!entry_at(&h.b, "note.txt").materialized);

    // Once stored, the tick has nothing left to ask for.
    h.fetches.clear();
    h.tick_b(std::time::Instant::now() + std::time::Duration::from_secs(60));
    assert!(h.fetches.is_empty(), "{:?}", h.fetches);
}

#[test]
fn exclude_rule_drops_the_entry_without_deleting_the_peer() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[]);
    h.b.materialize_add("Personal", "skip", "exclude", &["code/secret.txt".into()])
        .unwrap();
    publish(&mut h, &[("secret.txt", b"hidden")]);

    assert!(
        h.b.entries("Personal", "code", true)
            .unwrap()
            .iter()
            .all(|entry| entry.key.path.as_str() != "secret.txt")
    );
    assert!(!h.mount_b.path().join("secret.txt").exists());
    h.b.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    assert_eq!(
        fs::read(h.mount_a.path().join("secret.txt")).unwrap(),
        b"hidden"
    );
}

#[test]
fn switching_a_rule_to_full_materializes_on_tick() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[]);
    h.b.materialize_add("Personal", "notes", "metadata", &["code/note.txt".into()])
        .unwrap();
    publish(&mut h, &[("note.txt", b"hello")]);
    assert!(!h.mount_b.path().join("note.txt").exists());

    h.b.materialize_remove("Personal", "notes").unwrap();
    h.fetches.clear();
    h.tick_b(std::time::Instant::now());
    assert_eq!(
        fs::read(h.mount_b.path().join("note.txt")).unwrap(),
        b"hello"
    );
    assert!(entry_at(&h.b, "note.txt").materialized);
    assert!(h.fetches.contains(&ObjectId::of(b"hello")));
}

#[test]
fn later_materialization_rule_wins() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[]);
    h.b.materialize_add("Personal", "data", "metadata", &["code/data/**".into()])
        .unwrap();
    h.b.materialize_add("Personal", "keep", "full", &["code/data/keep.txt".into()])
        .unwrap();
    publish(
        &mut h,
        &[("data/skip.txt", b"skip"), ("data/keep.txt", b"keep")],
    );

    assert_eq!(
        fs::read(h.mount_b.path().join("data/keep.txt")).unwrap(),
        b"keep"
    );
    assert!(entry_at(&h.b, "data/keep.txt").materialized);
    assert!(!h.mount_b.path().join("data/skip.txt").exists());
    assert!(!entry_at(&h.b, "data/skip.txt").materialized);
}

#[test]
fn demand_fetches_updates_and_evicts_without_a_tombstone() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[]);
    h.b.materialize_add("Personal", "note", "demand", &["code/note.txt".into()])
        .unwrap();
    publish(&mut h, &[("note.txt", b"hello")]);

    let entry = entry_at(&h.b, "note.txt");
    assert!(!entry.materialized);
    assert!(!h.mount_b.path().join("note.txt").exists());
    assert!(!h.fetches.contains(&ObjectId::of(b"hello")));

    let mailbox = tempfile::tempdir().unwrap();
    h.b.set_replica_path(mailbox.path()).unwrap();
    let mut replica = FsReplica::open(mailbox.path()).unwrap();
    replica
        .put_object(ObjectId::of(b"hello"), b"hello")
        .unwrap();
    h.b.fetch_path("Personal", "code", "note.txt").unwrap();
    assert_eq!(
        fs::read(h.mount_b.path().join("note.txt")).unwrap(),
        b"hello"
    );
    assert!(entry_at(&h.b, "note.txt").materialized);

    fs::write(h.mount_a.path().join("note.txt"), b"edited").unwrap();
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    assert_eq!(
        fs::read(h.mount_b.path().join("note.txt")).unwrap(),
        b"edited"
    );
    assert!(entry_at(&h.b, "note.txt").materialized);

    h.b.evict_path("Personal", "code", "note.txt").unwrap();
    assert!(!h.mount_b.path().join("note.txt").exists());
    let evicted = entry_at(&h.b, "note.txt");
    assert!(!evicted.materialized);
    assert!(!evicted.is_deleted());
    let sequence = evicted.sequence;
    h.b.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    let after = entry_at(&h.b, "note.txt");
    assert!(!after.is_deleted());
    assert_eq!(after.sequence, sequence);
    h.push_both();
    assert_eq!(
        fs::read(h.mount_a.path().join("note.txt")).unwrap(),
        b"edited"
    );
}

#[test]
fn evict_refuses_when_file_bytes_differ() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[]);
    h.b.materialize_add("Personal", "note", "demand", &["code/note.txt".into()])
        .unwrap();
    publish(&mut h, &[("note.txt", b"hello")]);
    let mailbox = tempfile::tempdir().unwrap();
    h.b.set_replica_path(mailbox.path()).unwrap();
    let mut replica = FsReplica::open(mailbox.path()).unwrap();
    replica
        .put_object(ObjectId::of(b"hello"), b"hello")
        .unwrap();
    h.b.fetch_path("Personal", "code", "note.txt").unwrap();

    fs::write(h.mount_b.path().join("note.txt"), b"dirty").unwrap();
    let err = h.b.evict_path("Personal", "code", "note.txt").unwrap_err();
    assert!(err.to_string().contains("does not match"), "{err}");
    assert_eq!(
        fs::read(h.mount_b.path().join("note.txt")).unwrap(),
        b"dirty"
    );
    assert!(entry_at(&h.b, "note.txt").materialized);
}

#[test]
fn materialization_rule_crud() {
    let home = tempfile::tempdir().unwrap();
    let mut engine = Engine::init(home.path(), "alpha").unwrap();
    engine.create_space("Personal").unwrap();

    let missing = engine
        .materialize_add("Missing", "r", "metadata", &["code/**".into()])
        .unwrap_err();
    assert!(
        matches!(missing, relay_engine::EngineError::UnknownSpace(_)),
        "{missing}"
    );

    let empty = engine
        .materialize_add("Personal", "r", "metadata", &[])
        .unwrap_err();
    assert!(
        matches!(empty, relay_engine::EngineError::EmptyMaterialization),
        "{empty}"
    );

    let bad_glob = engine
        .materialize_add("Personal", "r", "metadata", &["[".into()])
        .unwrap_err();
    assert!(
        matches!(bad_glob, relay_engine::EngineError::Policy(_)),
        "{bad_glob}"
    );

    let added = engine
        .materialize_add("Personal", "r", "metadata", &["code/**".into()])
        .unwrap();
    assert_eq!(added.mode, "metadata");
    assert_eq!(added.selectors, vec!["code/**".to_owned()]);

    let duplicate = engine
        .materialize_add("Personal", "r", "full", &["code/a.txt".into()])
        .unwrap_err();
    assert!(
        matches!(
            duplicate,
            relay_engine::EngineError::DuplicateMaterialization(_)
        ),
        "{duplicate}"
    );

    let listed = engine.materialization_rules(Some("Personal")).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "r");

    engine.materialize_remove("Personal", "r").unwrap();
    assert!(
        engine
            .materialization_rules(Some("Personal"))
            .unwrap()
            .is_empty()
    );
    let gone = engine.materialize_remove("Personal", "r").unwrap_err();
    assert!(
        matches!(gone, relay_engine::EngineError::UnknownMaterialization(_)),
        "{gone}"
    );
}

// ---- Regressions from the sync review ----

fn entry_any(engine: &Engine, path: &str) -> relay_engine::EntryRecord {
    engine
        .entries("Personal", "code", true)
        .unwrap()
        .into_iter()
        .find(|e| e.key.path.as_str() == path)
        .unwrap_or_else(|| panic!("missing {path}"))
}

fn mount_id(engine: &Engine, name: &str) -> relay_core::MountId {
    engine
        .mounts(Some("Personal"))
        .unwrap()
        .into_iter()
        .map(|(_, c)| c)
        .find(|c| c.mount.name == name)
        .unwrap_or_else(|| panic!("missing mount {name}"))
        .mount
        .id
}

fn assert_converged(h: &Harness) {
    assert_eq!(
        index_triples(&h.a, "Personal", "code"),
        index_triples(&h.b, "Personal", "code")
    );
    assert_eq!(live_files(h.mount_a.path()), live_files(h.mount_b.path()));
}

#[test]
fn remote_tombstone_does_not_delete_an_unindexed_file() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("notes.txt", b"v1")]);
    fs::remove_file(h.mount_a.path().join("notes.txt")).unwrap();
    scan_allow_mass(&mut h.a);
    h.push_both();
    assert!(!h.mount_b.path().join("notes.txt").exists());
    assert!(entry_any(&h.b, "notes.txt").is_deleted());

    // B's user recreates the path before any scan sees it. Meanwhile A
    // recreates and deletes it again, so a newer tombstone arrives for a
    // path whose only index row is a tombstone.
    fs::write(h.mount_b.path().join("notes.txt"), b"mine").unwrap();
    fs::write(h.mount_a.path().join("notes.txt"), b"v2").unwrap();
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    fs::remove_file(h.mount_a.path().join("notes.txt")).unwrap();
    scan_allow_mass(&mut h.a);
    h.push_both();
    assert_eq!(
        fs::read(h.mount_b.path().join("notes.txt")).unwrap(),
        b"mine"
    );
    assert!(!entry_any(&h.b, "notes.txt").is_deleted());
    h.push_both();
    assert_eq!(
        fs::read(h.mount_a.path().join("notes.txt")).unwrap(),
        b"mine"
    );
    assert_converged(&h);
}

#[test]
fn file_replaced_by_directory_on_the_receiver() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("thing", b"flat")]);
    fs::remove_file(h.mount_a.path().join("thing")).unwrap();
    write_tree(h.mount_a.path(), &[("thing/inner.txt", b"inner")]);
    scan_allow_mass(&mut h.a);
    h.push_both();
    assert!(h.mount_b.path().join("thing").is_dir());
    assert_eq!(
        fs::read(h.mount_b.path().join("thing/inner.txt")).unwrap(),
        b"inner"
    );
    assert_converged(&h);
}

#[test]
fn directory_replaced_by_file_on_the_receiver() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("thing/inner.txt", b"inner")]);
    fs::remove_dir_all(h.mount_a.path().join("thing")).unwrap();
    fs::write(h.mount_a.path().join("thing"), b"flat").unwrap();
    scan_allow_mass(&mut h.a);
    h.push_both();
    assert!(h.mount_b.path().join("thing").is_file());
    assert_eq!(fs::read(h.mount_b.path().join("thing")).unwrap(), b"flat");
    assert_converged(&h);
}

#[cfg(unix)]
#[test]
fn symlink_replaced_by_file_and_back_on_the_receiver() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("target.txt", b"t"), ("link", b"flat")]);
    let link_a = h.mount_a.path().join("link");
    let link_b = h.mount_b.path().join("link");
    fs::remove_file(&link_a).unwrap();
    std::os::unix::fs::symlink("target.txt", &link_a).unwrap();
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    assert!(
        fs::symlink_metadata(&link_b)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_link(&link_b).unwrap(), Path::new("target.txt"));
    assert_eq!(
        index_triples(&h.a, "Personal", "code"),
        index_triples(&h.b, "Personal", "code")
    );

    fs::remove_file(&link_a).unwrap();
    fs::write(&link_a, b"back").unwrap();
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.push_both();
    let meta = fs::symlink_metadata(&link_b).unwrap();
    assert!(meta.is_file() && !meta.file_type().is_symlink());
    assert_eq!(fs::read(&link_b).unwrap(), b"back");
    assert_converged(&h);
}

/// A destination that cannot be written stalls its batch; after the retries
/// the batch is given up with a hole at the stalled entry, so neither it nor
/// the entries behind it are lost once the destination is writable again.
#[cfg(unix)]
#[test]
fn locked_destination_is_re_requested_not_dropped() {
    use std::os::unix::fs::PermissionsExt;
    let mut h = Harness::pair();
    h.setup_shared_space(&[("locked/existing.txt", b"e")]);
    let locked = h.mount_b.path().join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::write(locked.join(".probe"), b"").is_ok() {
        // Directory permissions do not bind this user (root).
        let _ = fs::remove_file(locked.join(".probe"));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let before = h.received(false);
    write_tree(
        h.mount_a.path(),
        &[("locked/new.txt", b"n"), ("zzz.txt", b"z")],
    );
    h.a.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.strict = false;
    h.push_both();
    // Two retries (5 s apart, on the wall clock) and the batch is given up.
    for _ in 0..2 {
        std::thread::sleep(std::time::Duration::from_millis(5_200));
        h.tick_b(std::time::Instant::now());
    }
    assert!(!locked.join("new.txt").exists());
    assert!(!h.mount_b.path().join("zzz.txt").exists());
    assert_eq!(
        h.received(false),
        before,
        "the watermark must not pass the stalled entry"
    );
    assert!(
        h.events.iter().any(|e| matches!(
            e,
            SyncEvent::SyncWarning { reason, .. } if reason.contains("giving up")
        )),
        "{:?}",
        h.events
    );

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    h.strict = true;
    h.events.clear();
    h.tick_b(std::time::Instant::now() + std::time::Duration::from_secs(31));
    assert_eq!(fs::read(locked.join("new.txt")).unwrap(), b"n");
    assert_eq!(fs::read(h.mount_b.path().join("zzz.txt")).unwrap(), b"z");
    assert!(h.received(false) > before);
    assert_converged(&h);
}

/// A metadata-only device resolves concurrent versions the same way the
/// devices holding the files do, so a hub does not keep the index churning.
#[test]
fn index_only_hub_resolves_concurrent_edits_without_churn() {
    let mut h = Hub::new();
    hub_fully_mesh(&mut h);
    h.engines[0].create_space("Personal").unwrap();
    h.engines[0]
        .add_mount("Personal", "code", h.mounts[0].path(), &[], &[])
        .unwrap();
    write_tree(h.mounts[0].path(), &[("doc.txt", b"base")]);
    h.engines[0]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.engines[0].share("Personal", "bravo").unwrap();
    h.engines[0].share("Personal", "charlie").unwrap();

    h.connect_pair(0, 1);
    h.engines[1].join_space("Personal", "alpha").unwrap();
    h.engines[1]
        .add_mount("Personal", "code", h.mounts[1].path(), &[], &[])
        .unwrap();
    h.engines[1]
        .materialize_add("Personal", "hub", "metadata", &["code/**".into()])
        .unwrap();
    h.engines[1].share("Personal", "charlie").unwrap();

    h.connect_pair(0, 2);
    h.engines[2].join_space("Personal", "alpha").unwrap();
    h.engines[2]
        .add_mount("Personal", "code", h.mounts[2].path(), &[], &[])
        .unwrap();
    // Bravo holds no objects, so charlie meets alpha before bravo.
    for i in 0..3 {
        h.syncers[i] = Syncer::new();
    }
    h.connect_pair(0, 1);
    h.connect_pair(0, 2);
    h.connect_pair(1, 2);
    h.push_all();
    assert_eq!(
        fs::read(h.mounts[2].path().join("doc.txt")).unwrap(),
        b"base"
    );
    assert!(!h.mounts[1].path().join("doc.txt").exists());

    fs::write(h.mounts[0].path().join("doc.txt"), b"alpha-edit").unwrap();
    fs::write(h.mounts[2].path().join("doc.txt"), b"charlie-edit").unwrap();
    h.engines[0]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.engines[2]
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    h.events.clear();
    h.conflicts_by_engine = [0; 3];
    h.push_all();

    // At most one resolution per device: the winner carries the merged
    // vector, so a device that sees the hub's resolution first applies it
    // without resolving again, and the hub's rows match the writers' rows as
    // soon as the first round goes quiet, so nothing is re-resolved later.
    let resolved = h.conflicts_by_engine;
    assert!(
        resolved.iter().all(|&n| n <= 1) && resolved.iter().sum::<usize>() >= 1,
        "{resolved:?} {:?}",
        h.events
    );
    let index = index_triples(&h.engines[0], "Personal", "code");
    assert_eq!(index, index_triples(&h.engines[1], "Personal", "code"));
    assert_eq!(index, index_triples(&h.engines[2], "Personal", "code"));
    h.events.clear();
    h.conflicts_by_engine = [0; 3];
    h.push_all();
    assert_eq!(h.conflicts_by_engine, [0; 3], "{:?}", h.events);
    assert_eq!(
        live_files(h.mounts[0].path()),
        live_files(h.mounts[2].path())
    );
    for i in 0..3 {
        assert_eq!(
            h.engines[i].conflicts(None).unwrap().len(),
            1,
            "{} should hold one conflict copy",
            Hub::name(i)
        );
    }
    assert!(live_files(h.mounts[1].path()).is_empty());
    assert!(
        h.engines[1]
            .entries("Personal", "code", false)
            .unwrap()
            .iter()
            .all(|e| !e.materialized)
    );
    h.push_all();
    assert_eq!(index, index_triples(&h.engines[0], "Personal", "code"));
    assert_eq!(index, index_triples(&h.engines[1], "Personal", "code"));
}

/// A mount a peer adds after this device joined is known under the peer's
/// id before it is attached, and attaching it replays the entries that
/// arrived while it was not.
#[test]
fn mount_offered_after_join_attaches_with_its_history() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[("a.txt", b"a")]);
    let docs_a = TempDir::new().unwrap();
    let docs_b = TempDir::new().unwrap();
    h.a.add_mount("Personal", "docs", docs_a.path(), &[], &[])
        .unwrap();
    write_tree(docs_a.path(), &[("readme.md", b"r")]);
    h.a.scan("Personal", "docs", ScanOptions::default())
        .unwrap();
    // The daemon re-offers on a mount change; a reconnect does the same here.
    // Entries for the unattached mount are skipped, with a warning.
    h.strict = false;
    h.reconnect();
    let offered =
        h.b.mounts(Some("Personal"))
            .unwrap()
            .into_iter()
            .map(|(_, c)| c)
            .find(|c| c.mount.name == "docs")
            .expect("an offered mount is known before it is attached");
    assert_eq!(offered.mount.id, mount_id(&h.a, "docs"));
    assert!(offered.local_path.is_none());
    assert!(!docs_b.path().join("readme.md").exists());

    h.strict = true;
    h.b.add_mount("Personal", "docs", docs_b.path(), &[], &[])
        .unwrap();
    assert_eq!(mount_id(&h.b, "docs"), mount_id(&h.a, "docs"));
    h.reconnect();
    assert_eq!(fs::read(docs_b.path().join("readme.md")).unwrap(), b"r");
    assert_eq!(
        index_triples(&h.a, "Personal", "docs"),
        index_triples(&h.b, "Personal", "docs")
    );
}

#[test]
fn hydration_does_not_overwrite_an_unscanned_file() {
    let mut h = Harness::pair();
    h.setup_shared_space(&[]);
    h.b.materialize_add("Personal", "note", "demand", &["code/note.txt".into()])
        .unwrap();
    publish(&mut h, &[("note.txt", b"hello")]);
    let mailbox = tempfile::tempdir().unwrap();
    h.b.set_replica_path(mailbox.path()).unwrap();
    let mut replica = FsReplica::open(mailbox.path()).unwrap();
    replica
        .put_object(ObjectId::of(b"hello"), b"hello")
        .unwrap();

    // The user wrote the path before asking for it; nothing has scanned it.
    fs::write(h.mount_b.path().join("note.txt"), b"mine").unwrap();
    let err = h.b.fetch_path("Personal", "code", "note.txt").unwrap_err();
    assert!(
        matches!(err, relay_engine::EngineError::DestinationChanged(_)),
        "{err}"
    );
    assert_eq!(
        fs::read(h.mount_b.path().join("note.txt")).unwrap(),
        b"mine"
    );
    assert!(!entry_at(&h.b, "note.txt").materialized);

    // The scan records the edit as this device's version.
    h.b.scan("Personal", "code", ScanOptions::default())
        .unwrap();
    assert!(entry_at(&h.b, "note.txt").materialized);
    h.push_both();
    h.push_both();
    assert_eq!(
        fs::read(h.mount_a.path().join("note.txt")).unwrap(),
        b"mine"
    );
    assert_converged(&h);
}
