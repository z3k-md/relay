use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use relay_core::remote::{RemoteCall, RemoteReply};
use relay_core::{ConfigApplied, ConfigChange, DeleteHoldDecision};
use relay_daemon::{DaemonEvent, DaemonOptions, HostKind};
use relay_engine::{Engine, WatchEvent, WatchOptions};
use relay_ipc::{PairJoinParams, PairStartParams};
use tempfile::TempDir;

const CONVERGE: Duration = Duration::from_secs(15);

struct DaemonSession {
    stop: Arc<AtomicBool>,
    events: Receiver<DaemonEvent>,
    handle: JoinHandle<anyhow::Result<()>>,
}

fn watch_opts() -> WatchOptions {
    WatchOptions {
        debounce: Duration::from_millis(50),
        max_batch_delay: Duration::from_millis(500),
        full_scan_interval: Duration::from_secs(600),
        use_watcher: true,
        ..WatchOptions::default()
    }
}

fn start_daemon(home: &Path) -> DaemonSession {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let home_path = home.to_path_buf();
    let handle = thread::spawn(move || {
        let opts = DaemonOptions {
            listen: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            watch: watch_opts(),
            verbose: false,
            host: HostKind::Cli,
            enable_stun: false,
            loopback_only: true,
            placeholders: false,
        };
        relay_daemon::run(&home_path, opts, &stop_thread, &mut |event| {
            let _ = tx.send(event.clone());
        })
    });
    DaemonSession {
        stop,
        events: rx,
        handle,
    }
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
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_started(session: &DaemonSession) -> Option<SocketAddr> {
    let deadline = Instant::now() + CONVERGE;
    while Instant::now() < deadline {
        if let Ok(event) = session.events.recv_timeout(Duration::from_millis(100))
            && let DaemonEvent::Started { listen, .. } = event
        {
            return Some(listen);
        }
    }
    None
}

fn drain(session: &DaemonSession) -> Vec<DaemonEvent> {
    let mut out = Vec::new();
    while let Ok(event) = session.events.try_recv() {
        out.push(event);
    }
    out
}

fn stop_daemon(session: DaemonSession) {
    session.stop.store(true, Ordering::SeqCst);
    session
        .handle
        .join()
        .expect("daemon thread")
        .expect("daemon");
}

fn live_has(home: &Path, space: &str, mount: &str, path: &str) -> bool {
    let Ok(engine) = Engine::open_read_only(home) else {
        return false;
    };
    let Ok(entries) = engine.entries(space, mount, false) else {
        return false;
    };
    entries.iter().any(|e| e.key.path.as_str() == path)
}

#[test]
fn reloads_when_another_process_adds_a_mount() {
    let home = TempDir::new().unwrap();
    let mount_a = TempDir::new().unwrap();
    let mount_b = TempDir::new().unwrap();
    fs::write(mount_a.path().join("a.txt"), b"aaa").unwrap();
    fs::write(mount_b.path().join("b.txt"), b"bbb").unwrap();

    {
        let mut engine = Engine::init(home.path(), "testdev").unwrap();
        engine.create_space("Personal").unwrap();
        engine
            .add_mount("Personal", "one", mount_a.path(), &[], &[])
            .unwrap();
    }

    let session = start_daemon(home.path());
    assert!(wait_started(&session).is_some(), "daemon did not start");
    assert!(
        wait_until(CONVERGE, || live_has(
            home.path(),
            "Personal",
            "one",
            "a.txt"
        )),
        "initial mount was not indexed"
    );
    drain(&session);

    {
        let mut other =
            Engine::open_for_config(home.path()).expect("writer lock released during run");
        other
            .add_mount("Personal", "two", mount_b.path(), &[], &[])
            .unwrap();
    }

    let mut saw_reloading = false;
    let mut saw_started_again = false;
    let deadline = Instant::now() + CONVERGE;
    while Instant::now() < deadline && !(saw_reloading && saw_started_again) {
        match session.events.recv_timeout(Duration::from_millis(200)) {
            Ok(DaemonEvent::Reloading) => saw_reloading = true,
            Ok(DaemonEvent::Started { .. }) if saw_reloading => saw_started_again = true,
            Ok(DaemonEvent::Started { .. }) => {}
            Ok(_) | Err(_) => {}
        }
    }
    assert!(saw_reloading, "expected Reloading after external mount add");
    assert!(saw_started_again, "expected Started after reload");
    assert!(
        wait_until(CONVERGE, || live_has(
            home.path(),
            "Personal",
            "two",
            "b.txt"
        )),
        "new mount file was not indexed after reload"
    );

    stop_daemon(session);
}

#[test]
fn does_not_reload_on_own_writes() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    fs::write(mount.path().join("seed.txt"), b"seed").unwrap();

    {
        let mut engine = Engine::init(home.path(), "testdev").unwrap();
        engine.create_space("Personal").unwrap();
        engine
            .add_mount("Personal", "code", mount.path(), &[], &[])
            .unwrap();
    }

    let session = start_daemon(home.path());
    assert!(wait_started(&session).is_some(), "daemon did not start");
    assert!(
        wait_until(CONVERGE, || live_has(
            home.path(),
            "Personal",
            "code",
            "seed.txt"
        )),
        "seed was not indexed"
    );

    fs::write(mount.path().join("later.txt"), b"later").unwrap();
    assert!(
        wait_until(CONVERGE, || live_has(
            home.path(),
            "Personal",
            "code",
            "later.txt"
        )),
        "filesystem change was not indexed"
    );

    thread::sleep(Duration::from_secs(3));
    let events = drain(&session);
    assert!(
        events.iter().all(|e| !matches!(e, DaemonEvent::Reloading)),
        "own scans / filesystem indexing must not emit Reloading: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DaemonEvent::Watch(WatchEvent::Scanned { .. })))
            || live_has(home.path(), "Personal", "code", "later.txt"),
        "expected the daemon to keep indexing without reloading"
    );

    stop_daemon(session);
}

fn wait_ipc(home: &Path) -> relay_ipc::Client {
    let deadline = Instant::now() + CONVERGE;
    while Instant::now() < deadline {
        if let Ok(Some(client)) = relay_ipc::Client::connect(home) {
            return client;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("IPC client did not connect");
}

#[test]
fn ipc_hello_status_rescan_pause_and_host_lock() {
    let home = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    fs::write(mount.path().join("seed.txt"), b"seed").unwrap();

    {
        let mut engine = Engine::init(home.path(), "testdev").unwrap();
        engine.create_space("Personal").unwrap();
        engine
            .add_mount("Personal", "code", mount.path(), &[], &[])
            .unwrap();
    }

    let session = start_daemon(home.path());
    assert!(wait_started(&session).is_some(), "daemon did not start");

    let mut client = wait_ipc(home.path());
    let hello = client.hello().expect("hello");
    assert_eq!(hello.host, HostKind::Cli);
    assert_eq!(hello.pid, std::process::id());
    assert_eq!(hello.protocol, relay_ipc::PROTOCOL_VERSION);

    let status = client.status().expect("status");
    assert_eq!(status.state, relay_ipc::HostState::Running);
    assert!(
        status
            .mounts
            .iter()
            .any(|m| m.space == "Personal" && m.mount == "code"),
        "status mounts: {:?}",
        status.mounts
    );

    assert!(
        wait_until(CONVERGE, || live_has(
            home.path(),
            "Personal",
            "code",
            "seed.txt"
        )),
        "seed was not indexed"
    );

    fs::write(mount.path().join("via-rescan.txt"), b"rescanned").unwrap();
    let queued = client.rescan(None, None).expect("rescan");
    assert!(
        queued.queued.iter().any(|n| n == "Personal/code"),
        "queued: {:?}",
        queued.queued
    );
    assert!(
        wait_until(CONVERGE, || live_has(
            home.path(),
            "Personal",
            "code",
            "via-rescan.txt"
        )),
        "rescan did not index the new file"
    );

    client.pause().expect("pause");
    assert!(
        wait_until(CONVERGE, || Engine::open(home.path()).is_ok()),
        "engine locks were not released after pause"
    );

    let mut client = wait_ipc(home.path());
    client.resume().expect("resume");
    assert!(
        wait_until(CONVERGE, || matches!(
            relay_ipc::Client::connect(home.path())
                .ok()
                .flatten()
                .and_then(|mut c| c.status().ok())
                .map(|s| s.state),
            Some(relay_ipc::HostState::Running)
        )),
        "did not resume"
    );

    let home_path = home.path().to_path_buf();
    let second = thread::spawn(move || {
        let stop = Arc::new(AtomicBool::new(false));
        relay_daemon::run(
            &home_path,
            DaemonOptions {
                listen: "127.0.0.1:0".parse().unwrap(),
                watch: watch_opts(),
                verbose: false,
                host: HostKind::Cli,
                enable_stun: false,
                loopback_only: true,
                placeholders: false,
            },
            &stop,
            &mut |_| {},
        )
    });
    let err = second
        .join()
        .expect("second host thread")
        .expect_err("second host");
    let message = format!("{err:#}");
    assert!(
        message.contains("another Relay host is already running"),
        "{message}"
    );
    assert!(
        message.contains("cli") || message.contains("pid"),
        "{message}"
    );

    stop_daemon(session);
}

#[test]
fn ipc_add_mount_and_share_without_reload() {
    let home = TempDir::new().unwrap();
    let mount_a = TempDir::new().unwrap();
    let mount_b = TempDir::new().unwrap();
    fs::write(mount_a.path().join("a.txt"), b"aaa").unwrap();
    fs::write(mount_b.path().join("b.txt"), b"bbb").unwrap();

    let peer_id = {
        let mut engine = Engine::init(home.path(), "alice").unwrap();
        engine.create_space("Personal").unwrap();
        engine
            .add_mount("Personal", "one", mount_a.path(), &[], &[])
            .unwrap();
        let peer = relay_core::DeviceId::random();
        engine
            .upsert_peer("bob", peer, &["127.0.0.1:9".to_owned()])
            .unwrap();
        peer
    };

    let session = start_daemon(home.path());
    assert!(wait_started(&session).is_some(), "daemon did not start");
    assert!(
        wait_until(CONVERGE, || live_has(
            home.path(),
            "Personal",
            "one",
            "a.txt"
        )),
        "initial mount was not indexed"
    );
    drain(&session);

    let mut client = wait_ipc(home.path());
    let added = client
        .config(&ConfigChange::AddMount {
            space: "Personal".into(),
            mount: "two".into(),
            path: mount_b.path().to_path_buf(),
            includes: Vec::new(),
            excludes: Vec::new(),
        })
        .expect("add_mount via ipc");
    let ConfigApplied::Mount { mount, path } = added else {
        panic!("unexpected result {added:?}");
    };
    assert_eq!(mount.name, "two");
    assert!(path.is_some());

    client
        .config(&ConfigChange::Share {
            space: "Personal".into(),
            peer: "bob".into(),
        })
        .expect("share via ipc");

    assert!(
        wait_until(CONVERGE, || live_has(
            home.path(),
            "Personal",
            "two",
            "b.txt"
        )),
        "ipc-added mount was not indexed without reload"
    );

    let engine = Engine::open_read_only(home.path()).unwrap();
    let status = engine.status().unwrap();
    let bob = status
        .peers
        .iter()
        .find(|p| p.name == "bob")
        .expect("bob peer in status");
    assert!(
        bob.spaces.iter().any(|s| s.space == "Personal"),
        "Personal should be shared with bob: {:?}",
        bob.spaces
    );
    assert_eq!(bob.id, peer_id);

    let reloads = drain(&session)
        .into_iter()
        .filter(|e| matches!(e, DaemonEvent::Reloading))
        .count();
    assert_eq!(reloads, 0, "live add_mount/share must not reload the host");

    stop_daemon(session);
}

#[test]
fn pair_via_ipc_shares_and_syncs_without_reload() {
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let mount_a = TempDir::new().unwrap();
    let mount_b = TempDir::new().unwrap();
    fs::write(mount_a.path().join("hello.txt"), b"from-a").unwrap();

    {
        let mut engine = Engine::init(home_a.path(), "alice").unwrap();
        engine.create_space("S").unwrap();
        engine
            .add_mount("S", "docs", mount_a.path(), &[], &[])
            .unwrap();
    }
    Engine::init(home_b.path(), "bob").unwrap();

    let session_a = start_daemon(home_a.path());
    let session_b = start_daemon(home_b.path());
    let addr_a = wait_started(&session_a).expect("alice started");
    assert!(wait_started(&session_b).is_some(), "bob started");
    drain(&session_a);
    drain(&session_b);

    let mut client_a = wait_ipc(home_a.path());
    let started = client_a
        .pair_start(&PairStartParams {
            share: vec!["S".to_owned()],
            allow_manage: false,
        })
        .expect("pair_start");
    assert_eq!(
        started.code.chars().filter(|c| c.is_ascii_digit()).count(),
        10
    );

    let mut client_b = wait_ipc(home_b.path());
    let joined = client_b
        .pair_join(&PairJoinParams {
            code: started.code.clone(),
            addr: Some(addr_a.to_string()),
            allow_manage: false,
        })
        .expect("pair_join");
    assert_eq!(joined.peer_name, "alice");

    assert!(
        wait_until(CONVERGE, || {
            let a = Engine::open_read_only(home_a.path()).ok();
            let b = Engine::open_read_only(home_b.path()).ok();
            match (a, b) {
                (Some(a), Some(b)) => {
                    let ap = a.peers().unwrap_or_default();
                    let bp = b.peers().unwrap_or_default();
                    ap.iter().any(|p| p.name == "bob") && bp.iter().any(|p| p.name == "alice")
                }
                _ => false,
            }
        }),
        "peers were not recorded after pairing"
    );

    assert!(
        wait_until(CONVERGE, || {
            Engine::open_read_only(home_b.path())
                .ok()
                .and_then(|e| e.offers().ok())
                .is_some_and(|offers| offers.iter().any(|o| o.name == "S"))
        }),
        "bob did not see offer for S"
    );

    let reloads_a = drain(&session_a)
        .into_iter()
        .filter(|e| matches!(e, DaemonEvent::Reloading))
        .count();
    let reloads_b = drain(&session_b)
        .into_iter()
        .filter(|e| matches!(e, DaemonEvent::Reloading))
        .count();
    assert_eq!(reloads_a, 0, "alice reloaded during pairing");
    assert_eq!(reloads_b, 0, "bob reloaded during pairing");

    {
        let mut engine = Engine::open_for_config(home_b.path()).unwrap();
        engine.join_space("S", "alice").unwrap();
        engine
            .add_mount("S", "docs", mount_b.path(), &[], &[])
            .unwrap();
    }

    assert!(
        wait_until(CONVERGE, || mount_b.path().join("hello.txt").exists()
            && fs::read(mount_b.path().join("hello.txt")).ok().as_deref()
                == Some(b"from-a")),
        "file did not sync after pairing"
    );

    stop_daemon(session_a);
    stop_daemon(session_b);
}

fn reload_count(session: &DaemonSession) -> usize {
    drain(session)
        .into_iter()
        .filter(|e| matches!(e, DaemonEvent::Reloading))
        .count()
}

/// Pair alice (listening on `addr_a`) with bob, sharing nothing yet.
fn pair(home_a: &Path, addr_a: SocketAddr, home_b: &Path) {
    pair_granting(home_a, addr_a, home_b, false);
}

/// Pair alice with bob. `alice_allows_bob` lets bob manage alice.
fn pair_granting(home_a: &Path, addr_a: SocketAddr, home_b: &Path, alice_allows_bob: bool) {
    let code = wait_ipc(home_a)
        .pair_start(&PairStartParams {
            share: Vec::new(),
            allow_manage: alice_allows_bob,
        })
        .expect("pair_start")
        .code;
    wait_ipc(home_b)
        .pair_join(&PairJoinParams {
            code,
            addr: Some(addr_a.to_string()),
            allow_manage: false,
        })
        .expect("pair_join");
    assert!(
        wait_until(CONVERGE, || has_peer(home_a, "bob")
            && has_peer(home_b, "alice")),
        "pairing was not recorded on both sides"
    );
}

fn config(home: &Path, change: ConfigChange) -> ConfigApplied {
    wait_ipc(home)
        .config(&change)
        .unwrap_or_else(|err| panic!("{change:?}: {err}"))
}

fn has_peer(home: &Path, name: &str) -> bool {
    Engine::open_read_only(home)
        .and_then(|engine| engine.peers())
        .is_ok_and(|peers| peers.iter().any(|p| p.name == name))
}

fn synced(folder: &Path, name: &str, bytes: &[u8]) -> bool {
    fs::read(folder.join(name)).ok().as_deref() == Some(bytes)
}

/// Join, attach, share, and detach from another process all apply on the
/// running loop: the join waits for an offer that arrives later, files flow
/// without a reconnect, and neither host reloads.
#[test]
fn live_config_join_attach_and_remove_without_reload() {
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let mount_a = TempDir::new().unwrap();
    let mount_b = TempDir::new().unwrap();
    fs::write(mount_a.path().join("hello.txt"), b"from-a").unwrap();
    {
        let mut engine = Engine::init(home_a.path(), "alice").unwrap();
        engine.create_space("S").unwrap();
        engine
            .add_mount("S", "docs", mount_a.path(), &[], &[])
            .unwrap();
    }
    Engine::init(home_b.path(), "bob").unwrap();

    let session_a = start_daemon(home_a.path());
    let session_b = start_daemon(home_b.path());
    let addr_a = wait_started(&session_a).expect("alice started");
    assert!(wait_started(&session_b).is_some(), "bob started");

    pair(home_a.path(), addr_a, home_b.path());
    let mut client_a = wait_ipc(home_a.path());

    // Bob asks to join before Alice shares; the loop holds the join.
    let home_b_path = home_b.path().to_path_buf();
    let join = thread::spawn(move || {
        wait_ipc(&home_b_path).config(&ConfigChange::JoinSpace {
            space: "S".into(),
            from_peer: "alice".into(),
            wait_ms: 10_000,
        })
    });
    thread::sleep(Duration::from_millis(300));
    assert!(!join.is_finished(), "join should wait for the offer");
    client_a
        .config(&ConfigChange::Share {
            space: "S".into(),
            peer: "bob".into(),
        })
        .expect("share via ipc");
    let joined = join.join().unwrap().expect("join via ipc");
    assert!(matches!(joined, ConfigApplied::Space { ref space } if space.name == "S"));

    let mut client_b = wait_ipc(home_b.path());
    client_b
        .config(&ConfigChange::AddMount {
            space: "S".into(),
            mount: "docs".into(),
            path: mount_b.path().to_path_buf(),
            includes: Vec::new(),
            excludes: Vec::new(),
        })
        .expect("attach via ipc");
    assert!(
        wait_until(CONVERGE, || synced(mount_b.path(), "hello.txt", b"from-a")),
        "existing file did not arrive after a live attach"
    );

    client_b
        .config(&ConfigChange::RemoveMount {
            space: "S".into(),
            mount: "docs".into(),
        })
        .expect("remove via ipc");
    let status = client_b.status().expect("status");
    assert!(
        !status.mounts.iter().any(|m| m.space == "S"),
        "removed mount still listed: {:?}",
        status.mounts
    );
    fs::write(mount_a.path().join("later.txt"), b"later").unwrap();
    assert!(
        wait_until(CONVERGE, || live_has(
            home_a.path(),
            "S",
            "docs",
            "later.txt"
        )),
        "alice did not index the new file"
    );
    thread::sleep(Duration::from_secs(1));
    assert!(
        !mount_b.path().join("later.txt").exists(),
        "a detached folder must not receive files"
    );
    assert!(synced(mount_b.path(), "hello.txt", b"from-a"), "files stay");

    assert_eq!(reload_count(&session_a), 0, "alice reloaded");
    assert_eq!(reload_count(&session_b), 0, "bob reloaded");
    stop_daemon(session_a);
    stop_daemon(session_b);
}

/// A held mass delete resumes on the live loop once decided over IPC.
#[test]
fn live_delete_hold_decision_resumes_without_reload() {
    const FILES: usize = 30;
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let mount_a = TempDir::new().unwrap();
    let mount_b = TempDir::new().unwrap();
    for n in 0..FILES {
        fs::write(mount_a.path().join(format!("f{n}.txt")), b"x").unwrap();
    }
    {
        let mut engine = Engine::init(home_a.path(), "alice").unwrap();
        engine.create_space("S").unwrap();
        engine
            .add_mount("S", "docs", mount_a.path(), &[], &[])
            .unwrap();
    }
    Engine::init(home_b.path(), "bob").unwrap();
    let session_a = start_daemon(home_a.path());
    let session_b = start_daemon(home_b.path());
    let addr_a = wait_started(&session_a).expect("alice started");
    assert!(wait_started(&session_b).is_some(), "bob started");
    pair(home_a.path(), addr_a, home_b.path());
    config(
        home_a.path(),
        ConfigChange::Share {
            space: "S".into(),
            peer: "bob".into(),
        },
    );
    config(
        home_b.path(),
        ConfigChange::JoinSpace {
            space: "S".into(),
            from_peer: "alice".into(),
            wait_ms: 10_000,
        },
    );
    config(
        home_b.path(),
        ConfigChange::AddMount {
            space: "S".into(),
            mount: "docs".into(),
            path: mount_b.path().to_path_buf(),
            includes: Vec::new(),
            excludes: Vec::new(),
        },
    );
    let count = |dir: &Path| {
        fs::read_dir(dir)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name() != ".relay-mount")
            .count()
    };
    assert!(
        wait_until(CONVERGE, || count(mount_b.path()) == FILES),
        "files did not sync to bob"
    );
    drain(&session_b);

    // Alice deletes everything on purpose; bob holds it (D22).
    for n in 0..FILES {
        fs::remove_file(mount_a.path().join(format!("f{n}.txt"))).unwrap();
    }
    Engine::open_for_config(home_a.path())
        .unwrap()
        .scan(
            "S",
            "docs",
            relay_engine::ScanOptions {
                allow_mass_delete: true,
                dry_run: false,
            },
        )
        .unwrap();
    assert!(
        wait_until(CONVERGE, || Engine::open_read_only(home_b.path())
            .and_then(|e| e.delete_holds())
            .is_ok_and(|holds| !holds.is_empty())),
        "bob did not hold the mass delete"
    );
    assert_eq!(count(mount_b.path()), FILES, "held deletes were applied");

    let decided = config(
        home_b.path(),
        ConfigChange::DecideDeleteHold {
            space: "S".into(),
            mount: None,
            peer: None,
            decision: DeleteHoldDecision::Apply,
        },
    );
    assert_eq!(decided, ConfigApplied::Holds { decided: 1 });
    assert!(
        wait_until(CONVERGE, || count(mount_b.path()) == 0),
        "applied deletes did not go through live"
    );
    assert_eq!(reload_count(&session_b), 0, "bob reloaded");
    stop_daemon(session_a);
    stop_daemon(session_b);
}

/// Bob browses alice's folders over the network after alice granted it while
/// pairing; alice, without a grant from bob, is refused.
#[test]
fn granted_peer_browses_folders_over_ipc() {
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let folder = TempDir::new().unwrap();
    fs::create_dir(folder.path().join("Projects")).unwrap();
    fs::write(folder.path().join("notes.txt"), b"hi").unwrap();
    Engine::init(home_a.path(), "alice").unwrap();
    Engine::init(home_b.path(), "bob").unwrap();
    let session_a = start_daemon(home_a.path());
    let session_b = start_daemon(home_b.path());
    let addr_a = wait_started(&session_a).expect("alice started");
    assert!(wait_started(&session_b).is_some(), "bob started");
    pair_granting(home_a.path(), addr_a, home_b.path(), true);

    // The peer row and its grant are separate writes.
    assert!(
        wait_until(CONVERGE, || Engine::open_read_only(home_a.path())
            .and_then(|engine| engine.peers())
            .is_ok_and(|peers| peers
                .iter()
                .any(|p| p.name == "bob" && p.may_manage))),
        "alice never recorded bob's grant"
    );
    assert!(
        wait_until(CONVERGE, || wait_ipc(home_b.path()).status().is_ok_and(
            |s| s.peers.iter().any(|p| p.name == "alice" && p.manageable)
        )),
        "bob never learned alice's grant"
    );

    let path = folder.path().to_str().unwrap().to_owned();
    let reply = wait_ipc(home_b.path())
        .remote(
            "alice",
            &RemoteCall::ListDir {
                path,
                cursor: 0,
                limit: 0,
            },
        )
        .expect("list alice's folder");
    let RemoteReply::Listing { listing } = reply else {
        panic!("unexpected reply {reply:?}");
    };
    let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Projects", "notes.txt"]);

    // Folder sizes are counted on alice in the background and polled.
    let projects = listing.entries[0].path.clone();
    let mut done = false;
    for _ in 0..200 {
        let reply = wait_ipc(home_b.path())
            .remote(
                "alice",
                &RemoteCall::FolderSizes {
                    path: listing.path.clone(),
                },
            )
            .expect("folder sizes on alice");
        let RemoteReply::FolderSizes { sizes } = reply else {
            panic!("unexpected reply {reply:?}");
        };
        assert_eq!(sizes.folders.len(), 1);
        assert_eq!(sizes.folders[0].path, projects);
        if sizes.done {
            done = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(done, "folder sizes never finished");

    let refused = wait_ipc(home_a.path())
        .remote("bob", &RemoteCall::Roots)
        .unwrap_err();
    assert!(
        matches!(refused, relay_ipc::IpcError::Remote { ref code, .. } if code == "forbidden"),
        "{refused:?}"
    );

    // Taking the grant back is pushed to bob and enforced.
    config(
        home_a.path(),
        ConfigChange::SetPeerManage {
            peer: "bob".into(),
            allowed: false,
        },
    );
    assert!(
        wait_until(CONVERGE, || wait_ipc(home_b.path()).status().is_ok_and(
            |s| s.peers.iter().any(|p| p.name == "alice" && !p.manageable)
        )),
        "bob still thinks it may manage alice"
    );
    let refused = wait_ipc(home_b.path())
        .remote("alice", &RemoteCall::Roots)
        .unwrap_err();
    assert!(
        matches!(refused, relay_ipc::IpcError::Remote { ref code, .. } if code == "forbidden"),
        "{refused:?}"
    );
    assert_eq!(reload_count(&session_a), 0, "alice reloaded");
    stop_daemon(session_a);
    stop_daemon(session_b);
}

/// Online-only files: listed without bytes, fetched over IPC, freed again,
/// all on the live loop.
#[test]
fn online_only_files_fetch_and_free_without_reload() {
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let mount_a = TempDir::new().unwrap();
    let mount_b = TempDir::new().unwrap();
    fs::write(mount_a.path().join("report.docx"), b"quarterly").unwrap();
    {
        let mut engine = Engine::init(home_a.path(), "alice").unwrap();
        engine.create_space("S").unwrap();
        engine
            .add_mount("S", "docs", mount_a.path(), &[], &[])
            .unwrap();
    }
    Engine::init(home_b.path(), "bob").unwrap();
    let session_a = start_daemon(home_a.path());
    let session_b = start_daemon(home_b.path());
    let addr_a = wait_started(&session_a).expect("alice started");
    assert!(wait_started(&session_b).is_some(), "bob started");
    pair(home_a.path(), addr_a, home_b.path());
    config(
        home_a.path(),
        ConfigChange::Share {
            space: "S".into(),
            peer: "bob".into(),
        },
    );
    config(
        home_b.path(),
        ConfigChange::JoinSpace {
            space: "S".into(),
            from_peer: "alice".into(),
            wait_ms: 10_000,
        },
    );
    // Online only before attaching, so nothing downloads on its own.
    config(
        home_b.path(),
        ConfigChange::SetFolderMode {
            space: "S".into(),
            mount: "docs".into(),
            path: String::new(),
            mode: Some("demand".into()),
        },
    );
    config(
        home_b.path(),
        ConfigChange::AddMount {
            space: "S".into(),
            mount: "docs".into(),
            path: mount_b.path().to_path_buf(),
            includes: Vec::new(),
            excludes: Vec::new(),
        },
    );
    let state = || {
        Engine::open_read_only(home_b.path())
            .and_then(|e| e.list_folder("S", "docs", ""))
            .ok()
            .and_then(|view| view.entries.into_iter().find(|e| e.name == "report.docx"))
            .map(|e| e.state)
    };
    assert!(
        wait_until(CONVERGE, || state()
            == Some(relay_engine::CopyState::OnlineOnly)),
        "the file was not listed as online only: {:?}",
        state()
    );
    assert!(!mount_b.path().join("report.docx").exists());

    let mut client = wait_ipc(home_b.path());
    client.fetch("S", "docs", "report.docx").expect("fetch");
    assert_eq!(
        fs::read(mount_b.path().join("report.docx")).unwrap(),
        b"quarterly"
    );
    assert_eq!(state(), Some(relay_engine::CopyState::Local));

    assert_eq!(client.evict("S", "docs", "").expect("evict"), 1);
    assert!(!mount_b.path().join("report.docx").exists());
    assert_eq!(state(), Some(relay_engine::CopyState::OnlineOnly));
    assert!(
        fs::read(mount_a.path().join("report.docx")).is_ok(),
        "freeing space here leaves alice's copy"
    );
    assert_eq!(reload_count(&session_b), 0, "bob reloaded");
    stop_daemon(session_a);
    stop_daemon(session_b);
}

fn folder_pair_params(
    source: (Option<&str>, &Path),
    dest: (Option<&str>, &Path),
    create_dest: Option<&str>,
    excludes: &[&str],
) -> relay_ipc::FolderPairParams {
    let end = |(device, path): (Option<&str>, &Path)| relay_ipc::FolderEnd {
        device: device.map(str::to_owned),
        path: dunce_like(path),
    };
    relay_ipc::FolderPairParams {
        source: end(source),
        dest: end(dest),
        create_dest: create_dest.map(str::to_owned),
        name: None,
        excludes: excludes.iter().map(|s| (*s).to_owned()).collect(),
        exclude_patterns: Vec::new(),
        dest_online_only: false,
    }
}

/// Canonical the way Relay does it: `/var` and `/private/var` on macOS
/// compare equal, and Windows paths stay `C:\…` rather than `\\?\C:\…`.
fn canonical(path: &Path) -> std::path::PathBuf {
    dunce::canonicalize(path).unwrap()
}

fn dunce_like(path: &Path) -> String {
    canonical(path).to_str().unwrap().to_owned()
}

/// D39: bob, allowed to manage alice, pairs folders in
/// both directions from bob alone, and a failed setup leaves nothing behind.
#[test]
fn manager_sets_up_folder_pairs_both_ways() {
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let alice_folder = TempDir::new().unwrap();
    let bob_parent = TempDir::new().unwrap();
    let bob_folder = TempDir::new().unwrap();
    let alice_dest = TempDir::new().unwrap();
    fs::write(alice_folder.path().join("notes.txt"), b"from alice").unwrap();
    fs::create_dir(alice_folder.path().join("cache")).unwrap();
    fs::write(alice_folder.path().join("cache/big.bin"), b"skip me").unwrap();
    fs::write(bob_folder.path().join("todo.txt"), b"from bob").unwrap();
    Engine::init(home_a.path(), "alice").unwrap();
    Engine::init(home_b.path(), "bob").unwrap();
    let session_a = start_daemon(home_a.path());
    let session_b = start_daemon(home_b.path());
    let addr_a = wait_started(&session_a).expect("alice started");
    assert!(wait_started(&session_b).is_some(), "bob started");
    pair_granting(home_a.path(), addr_a, home_b.path(), true);
    assert!(
        wait_until(CONVERGE, || wait_ipc(home_b.path()).status().is_ok_and(
            |s| s.peers.iter().any(|p| p.name == "alice" && p.manageable)
        )),
        "bob never learned alice's grant"
    );
    let mut bob = wait_ipc(home_b.path());

    // A manager cannot change who alice trusts.
    let refused = bob
        .remote(
            "alice",
            &RemoteCall::Apply {
                change: ConfigChange::AddPeer {
                    peer: "mallory".into(),
                    id: relay_core::DeviceId::random(),
                    addresses: Vec::new(),
                },
            },
        )
        .unwrap_err();
    assert!(
        matches!(refused, relay_ipc::IpcError::Remote { ref code, .. } if code == "forbidden"),
        "{refused:?}"
    );

    // A failure part way through undoes the steps already taken on alice.
    let broken = folder_pair_params(
        (Some("alice"), alice_folder.path()),
        (None, bob_parent.path()),
        Some("bad/name"),
        &[],
    );
    assert!(bob.folder_pair(&broken).is_err());
    let alice_spaces = || {
        Engine::open_read_only(home_a.path())
            .unwrap()
            .spaces()
            .unwrap()
    };
    assert!(alice_spaces().is_empty(), "undo left {:?}", alice_spaces());
    assert!(!alice_folder.path().join(".relay-mount").exists());

    // Alice's folder into a new folder on bob, leaving out cache/.
    let params = folder_pair_params(
        (Some("alice"), alice_folder.path()),
        (None, bob_parent.path()),
        Some("xyz-foo"),
        &["cache"],
    );
    let plan = bob.folder_pair_preview(&params).expect("preview");
    assert!(plan.problems.is_empty(), "{:?}", plan.problems);
    let made = bob.folder_pair(&params).expect("folder pair");
    let dest = bob_parent.path().join("xyz-foo");
    assert!(
        wait_until(CONVERGE, || synced(&dest, "notes.txt", b"from alice")),
        "alice's file did not reach bob"
    );
    assert!(!dest.join("cache").exists(), "excluded folder synced");
    assert_eq!(made.space, plan.space);

    // Bob's own folder onto alice, set up from bob too.
    let params = folder_pair_params(
        (None, bob_folder.path()),
        (Some("alice"), alice_dest.path()),
        None,
        &[],
    );
    bob.folder_pair(&params).expect("second folder pair");
    assert!(
        wait_until(CONVERGE, || synced(
            alice_dest.path(),
            "todo.txt",
            b"from bob"
        )),
        "bob's file did not reach alice"
    );

    // The same source again is refused before anything changes.
    let again = bob.folder_pair_preview(&folder_pair_params(
        (Some("alice"), alice_folder.path()),
        (None, bob_parent.path()),
        Some("other"),
        &[],
    ));
    assert!(!again.expect("preview").problems.is_empty());

    assert_eq!(reload_count(&session_a), 0, "alice reloaded");
    assert_eq!(reload_count(&session_b), 0, "bob reloaded");
    stop_daemon(session_a);
    stop_daemon(session_b);
}

/// D40: bob opens alice's files whether or not they sync
/// anywhere, and removing a quick-open undoes only what it set up.
#[test]
fn open_remote_files_and_remove_quick_opens() {
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let docs = TempDir::new().unwrap();
    let synced = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    fs::write(docs.path().join("report.docx"), b"quarterly").unwrap();
    fs::write(docs.path().join("notes.txt"), b"notes").unwrap();
    fs::write(docs.path().join("~$report.docx"), b"lock").unwrap();
    fs::write(synced.path().join("plan.md"), b"plan").unwrap();
    {
        let mut engine = Engine::init(home_a.path(), "alice").unwrap();
        engine.create_space("Work").unwrap();
        engine
            .add_mount("Work", "work", synced.path(), &[], &[])
            .unwrap();
    }
    Engine::init(home_b.path(), "bob").unwrap();
    let session_a = start_daemon(home_a.path());
    let session_b = start_daemon(home_b.path());
    let addr_a = wait_started(&session_a).expect("alice started");
    assert!(wait_started(&session_b).is_some(), "bob started");
    pair_granting(home_a.path(), addr_a, home_b.path(), true);
    assert!(
        wait_until(CONVERGE, || wait_ipc(home_b.path()).status().is_ok_and(
            |s| s.peers.iter().any(|p| p.name == "alice" && p.manageable)
        )),
        "bob never learned alice's grant"
    );
    let mut bob = wait_ipc(home_b.path());
    let open_as = |client: &mut relay_ipc::Client, file: &Path, read_only: bool| {
        client
            .open_remote(&relay_ipc::OpenRemoteParams {
                peer: "alice".into(),
                path: dunce_like(file),
                root: Some(root.path().to_path_buf()),
                read_only,
            })
            .expect("open remote")
    };
    let open = |client: &mut relay_ipc::Client, file: &Path| open_as(client, file, false);
    let space = |opened: &relay_ipc::OpenedRemote| opened.synced.clone().unwrap().space;

    // A read-only copy sets nothing up on either device (D41).
    let copy = open_as(&mut bob, &docs.path().join("report.docx"), true);
    assert_eq!(copy.synced, None);
    assert_eq!(fs::read(&copy.path).unwrap(), b"quarterly");
    assert!(fs::metadata(&copy.path).unwrap().permissions().readonly());
    assert!(copy.path.starts_with(home_b.path().join("read-only")));
    assert!(
        Engine::open_read_only(home_b.path())
            .unwrap()
            .spaces()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        Engine::open_read_only(home_a.path())
            .unwrap()
            .spaces()
            .unwrap()
            .len(),
        1
    );
    let logged = wait_ipc(home_a.path())
        .activity(Some(20))
        .expect("activity");
    assert!(
        logged
            .iter()
            .any(|item| item.kind == "remote_change" && item.summary.starts_with("bob copied ")),
        "alice did not log the copy: {logged:?}"
    );

    // Case 3: in no synced folder.
    let opened = open(&mut bob, &docs.path().join("report.docx"));
    assert_eq!(fs::read(&opened.path).unwrap(), b"quarterly");
    assert!(
        opened
            .path
            .starts_with(canonical(root.path()).join("alice"))
    );
    let folder = opened.path.parent().unwrap().to_path_buf();
    assert!(
        !folder.join("notes.txt").exists(),
        "only the opened file downloads"
    );
    // A second file in the same folder reuses that pair.
    let notes = open(&mut bob, &docs.path().join("notes.txt"));
    assert_eq!(space(&notes), space(&opened));
    assert_eq!(fs::read(&notes.path).unwrap(), b"notes");
    // Edits flow back like any synced file.
    fs::write(&opened.path, b"edited on bob").unwrap();
    assert!(
        wait_until(CONVERGE, || synced_bytes(
            &docs.path().join("report.docx"),
            b"edited on bob"
        )),
        "bob's edit did not reach alice"
    );

    // Case 2: inside alice's own space, which bob does not sync.
    let plan = open(&mut bob, &synced.path().join("plan.md"));
    assert_eq!(space(&plan), "Work");
    assert_eq!(fs::read(&plan.path).unwrap(), b"plan");

    let quick = bob.quick_opens().expect("list");
    assert_eq!(quick.len(), 2, "{quick:?}");
    assert!(
        quick
            .iter()
            .any(|q| q.space == space(&opened) && q.created_on_peer)
    );
    assert!(
        quick
            .iter()
            .any(|q| q.space == "Work" && !q.created_on_peer)
    );

    // Removing undoes only what quick-open did.
    assert_eq!(
        bob.quick_open_remove(&space(&opened)).expect("remove"),
        None
    );
    assert_eq!(bob.quick_open_remove("Work").expect("remove"), None);
    assert!(bob.quick_opens().expect("list").is_empty());
    let alice = Engine::open_read_only(home_a.path()).unwrap();
    let names: Vec<_> = alice
        .spaces()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["Work"], "alice keeps her own space only");
    let work = alice
        .status()
        .unwrap()
        .peers
        .into_iter()
        .find(|p| p.name == "bob")
        .map(|p| p.spaces)
        .unwrap_or_default();
    assert!(
        work.is_empty(),
        "Work is no longer shared with bob: {work:?}"
    );
    assert!(fs::read(&opened.path).is_ok(), "files stay on bob's disk");
    assert_eq!(reload_count(&session_a), 0, "alice reloaded");
    assert_eq!(reload_count(&session_b), 0, "bob reloaded");
    stop_daemon(session_a);
    stop_daemon(session_b);
}

fn synced_bytes(path: &Path, bytes: &[u8]) -> bool {
    fs::read(path).ok().as_deref() == Some(bytes)
}
