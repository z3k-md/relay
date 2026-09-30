use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use relay_daemon::{DaemonEvent, DaemonOptions};
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
