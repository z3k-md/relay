//! Deterministic in-process two-engine sync harness.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use relay_core::conflict::conflict_path;
use relay_core::{DeviceId, EntryContent, LogicalPath, MOUNT_MARKER, ObjectId};
use relay_engine::{
    DeleteHoldDecision, Engine, EngineConfig, ManualClock, ScanOptions, SyncEvent, SyncInput,
    SyncOutput, Syncer,
};
use relay_fs::MountMarker;
use relay_proto::{IndexBatch, entry_to_wire, frame, space_id_bytes};
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
        self.sb
            .tick(&mut self.b, now, &mut |o| outs.push(o))
            .unwrap();
        let q = VecDeque::new();
        let mut outputs: VecDeque<(bool, SyncOutput)> =
            outs.into_iter().map(|o| (false, o)).collect();
        // Deliver B's outputs, then pump everything to quiescence.
        let mut q2 = q;
        while let Some((from_a, output)) = outputs.pop_front() {
            if let SyncOutput::Send { body, .. } = output {
                q2.push_back((
                    !from_a,
                    SyncInput::Frame {
                        peer: self.id_b(),
                        body,
                    },
                ));
            }
        }
        self.pump(q2, None);
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
                SyncOutput::SetPeers => {}
                SyncOutput::FetchObject { object, .. } => {
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
                SyncOutput::SetPeers => None,
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
