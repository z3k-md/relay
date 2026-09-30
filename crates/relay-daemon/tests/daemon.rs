use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use relay_daemon::{DaemonEvent, DaemonOptions, HostKind};
use relay_engine::{Engine, WatchEvent, WatchOptions};
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
    let started = client_a.pair_start(&["S".to_owned()]).expect("pair_start");
    assert_eq!(
        started.code.chars().filter(|c| c.is_ascii_digit()).count(),
        10
    );

    let mut client_b = wait_ipc(home_b.path());
    let joined = client_b
        .pair_join(&started.code, Some(&addr_a.to_string()))
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
