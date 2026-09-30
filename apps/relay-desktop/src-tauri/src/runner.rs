use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use relay_engine::{WatchEvent, WatchOptions};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::sidecar;
use crate::{settings, tray};
use relay_daemon::{DaemonEvent, DaemonOptions};

pub const DEFAULT_LISTEN: &str = "0.0.0.0:47321";
pub const DEFAULT_DEBOUNCE_MS: u64 = 200;
pub const DEFAULT_FULL_SCAN_SECS: u64 = 600;
const MAX_EVENTS: usize = 300;
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);

pub const EXTERNAL_SERVICE_MESSAGE: &str = "Sync is handled by the `relay service` background service, so this app will not run a second copy. To switch to in-app sync, run `relay service uninstall` and reopen Relay.";

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RunnerState {
    NotInitialized,
    Starting,
    Running,
    Paused,
    Error { message: String },
    ExternalService { message: String },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityItem {
    pub ts_ms: i64,
    pub kind: String,
    pub message: String,
}

struct Inner {
    state: RunnerState,
    events: VecDeque<ActivityItem>,
    connected: HashSet<String>,
    thread: Option<JoinHandle<()>>,
}

pub struct Runner {
    home: PathBuf,
    stop: Arc<AtomicBool>,
    inner: Mutex<Inner>,
}

impl Runner {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            stop: Arc::new(AtomicBool::new(false)),
            inner: Mutex::new(Inner {
                state: RunnerState::NotInitialized,
                events: VecDeque::new(),
                connected: HashSet::new(),
                thread: None,
            }),
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn state(&self) -> RunnerState {
        self.inner
            .lock()
            .map(|g| g.state.clone())
            .unwrap_or(RunnerState::Error {
                message: "internal lock poisoned".to_owned(),
            })
    }

    pub fn activity(&self) -> Vec<ActivityItem> {
        self.inner
            .lock()
            .map(|g| g.events.iter().rev().cloned().collect())
            .unwrap_or_default()
    }

    pub fn connected_peers(&self) -> HashSet<String> {
        self.inner
            .lock()
            .map(|g| g.connected.clone())
            .unwrap_or_default()
    }

    pub fn connected_count(&self) -> usize {
        self.inner.lock().map(|g| g.connected.len()).unwrap_or(0)
    }

    pub fn start(&self, app: &AppHandle) {
        if sidecar::service_is_running(&self.home) {
            self.set_state(
                app,
                RunnerState::ExternalService {
                    message: EXTERNAL_SERVICE_MESSAGE.to_owned(),
                },
            );
            return;
        }
        if !is_initialized(&self.home) {
            self.set_state(app, RunnerState::NotInitialized);
            return;
        }
        if settings::paused(app) {
            self.set_state(app, RunnerState::Paused);
            return;
        }

        self.stop_join();
        self.stop.store(false, Ordering::SeqCst);
        self.set_state(app, RunnerState::Starting);

        let home = self.home.clone();
        let stop = Arc::clone(&self.stop);
        let app = app.clone();
        let handle = thread::Builder::new()
            .name("relay-sync".to_owned())
            .spawn(move || sync_loop(home, stop, app))
            .expect("spawn sync thread");

        if let Ok(mut inner) = self.inner.lock() {
            inner.thread = Some(handle);
        }
    }

    pub fn stop_join(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let handle = self.inner.lock().ok().and_then(|mut g| g.thread.take());
        if let Some(handle) = handle {
            join_with_timeout(handle, JOIN_TIMEOUT);
        }
    }

    pub fn restart(&self, app: &AppHandle) {
        self.stop_join();
        self.start(app);
    }

    pub fn pause(&self, app: &AppHandle) -> anyhow::Result<()> {
        if matches!(self.state(), RunnerState::ExternalService { .. }) {
            anyhow::bail!("{EXTERNAL_SERVICE_MESSAGE}");
        }
        settings::set_paused(app, true)?;
        self.stop_join();
        self.set_state(app, RunnerState::Paused);
        Ok(())
    }

    pub fn resume(&self, app: &AppHandle) -> anyhow::Result<()> {
        if matches!(self.state(), RunnerState::ExternalService { .. }) {
            anyhow::bail!("{EXTERNAL_SERVICE_MESSAGE}");
        }
        settings::set_paused(app, false)?;
        self.start(app);
        Ok(())
    }

    fn set_state(&self, app: &AppHandle, state: RunnerState) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner.state == state {
                return;
            }
            inner.state = state.clone();
        }
        let _ = app.emit("relay://state", &state);
        tray::refresh(app);
    }

    fn push_event(&self, app: &AppHandle, item: ActivityItem) {
        if let Ok(mut inner) = self.inner.lock() {
            match item.kind.as_str() {
                "peerConnected" => {
                    if let Some(id) = item.message.split_whitespace().last() {
                        inner.connected.insert(id.to_owned());
                    }
                }
                "peerDisconnected" => {
                    if let Some(id) = item.message.split_whitespace().last() {
                        inner.connected.remove(id);
                    }
                }
                _ => {}
            }
            inner.events.push_back(item.clone());
            while inner.events.len() > MAX_EVENTS {
                inner.events.pop_front();
            }
        }
        let _ = app.emit("relay://activity", &item);
        tray::refresh(app);
    }

    fn apply_watch_peer(&self, event: &WatchEvent) {
        if let Ok(mut inner) = self.inner.lock() {
            match event {
                WatchEvent::PeerConnected { peer, .. } => {
                    inner.connected.insert(peer.clone());
                }
                WatchEvent::PeerDisconnected { peer } => {
                    inner.connected.remove(peer);
                }
                _ => {}
            }
        }
    }
}

fn sync_loop(home: PathBuf, stop: Arc<AtomicBool>, app: AppHandle) {
    let mut backoff = BACKOFF_MIN;
    while !stop.load(Ordering::SeqCst) {
        if sidecar::service_is_running(&home) {
            if let Some(runner) = app.try_state::<crate::AppState>() {
                runner.runner.set_state(
                    &app,
                    RunnerState::ExternalService {
                        message: EXTERNAL_SERVICE_MESSAGE.to_owned(),
                    },
                );
            }
            return;
        }

        let opts = DaemonOptions {
            listen: default_listen(),
            watch: WatchOptions {
                debounce: Duration::from_millis(DEFAULT_DEBOUNCE_MS),
                full_scan_interval: Duration::from_secs(DEFAULT_FULL_SCAN_SECS),
                reload_on_external_change: true,
                ..WatchOptions::default()
            },
            verbose: false,
        };

        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let mut on_event = |event: &DaemonEvent| handle_daemon_event(&app, event);
            relay_daemon::run(&home, opts, &stop, &mut on_event)
        }));

        match result {
            Ok(Ok(())) => return,
            Ok(Err(err)) => {
                let message = format!("{err:#}");
                record_error(&app, &message);
                if sleep_backoff(&stop, backoff) {
                    return;
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
            Err(payload) => {
                let message = panic_message(payload);
                record_error(&app, &message);
                if sleep_backoff(&stop, backoff) {
                    return;
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

fn handle_daemon_event(app: &AppHandle, event: &DaemonEvent) {
    let Some(state) = app.try_state::<crate::AppState>() else {
        return;
    };
    let runner = &state.runner;
    let (kind, message) = describe_event(event);
    if let DaemonEvent::Watch(watch) = event {
        runner.apply_watch_peer(watch);
    }
    if matches!(event, DaemonEvent::Started { .. }) {
        runner.set_state(app, RunnerState::Running);
    }
    runner.push_event(
        app,
        ActivityItem {
            ts_ms: now_ms(),
            kind,
            message,
        },
    );
}

fn describe_event(event: &DaemonEvent) -> (String, String) {
    match event {
        DaemonEvent::Started {
            device_name,
            device_id,
            listen,
        } => (
            "started".to_owned(),
            format!("Started as {device_name} ({device_id}) on {listen}"),
        ),
        DaemonEvent::Reloading => (
            "reloading".to_owned(),
            "Reloading after a database change".to_owned(),
        ),
        DaemonEvent::Warning(msg) => ("warning".to_owned(), msg.clone()),
        DaemonEvent::Watch(watch) => describe_watch(watch),
    }
}

fn describe_watch(event: &WatchEvent) -> (String, String) {
    match event {
        WatchEvent::Started { mounts } => {
            let msg = if mounts.is_empty() {
                "Watching (no local mounts)".to_owned()
            } else {
                format!("Watching {}", mounts.join(", "))
            };
            ("watch".to_owned(), msg)
        }
        WatchEvent::Scanned {
            space,
            mount,
            full,
            paths,
            report,
        } => {
            let kind = if *full { "full scan" } else { "scan" };
            (
                "scan".to_owned(),
                format!(
                    "{space}/{mount}: {kind}, {} created, {} modified, {} deleted ({paths} paths)",
                    report.created, report.modified, report.deleted
                ),
            )
        }
        WatchEvent::ScanFailed {
            space,
            mount,
            error,
        } => ("error".to_owned(), format!("{space}/{mount}: {error}")),
        WatchEvent::WatcherUnavailable {
            space,
            mount,
            error,
        } => (
            "warning".to_owned(),
            format!("{space}/{mount}: watcher unavailable: {error}"),
        ),
        WatchEvent::PeerConnected { peer, name } => (
            "peerConnected".to_owned(),
            format!("Connected to {name} {peer}"),
        ),
        WatchEvent::PeerDisconnected { peer } => (
            "peerDisconnected".to_owned(),
            format!("Disconnected from {peer}"),
        ),
        WatchEvent::OffersReceived { peer, spaces } => (
            "offer".to_owned(),
            format!("{peer} offers: {}", spaces.join(", ")),
        ),
        WatchEvent::RemoteApplied {
            peer,
            space,
            mount,
            written,
            deleted,
            conflicts,
            skipped,
        } => (
            "sync".to_owned(),
            format!(
                "{space}/{mount} from {peer}: {written} written, {deleted} deleted, {conflicts} conflicts, {skipped} skipped"
            ),
        ),
        WatchEvent::SentChanges {
            peer,
            space,
            entries,
        } => (
            "sync".to_owned(),
            format!("Sent {entries} changes of {space} to {peer}"),
        ),
        WatchEvent::SyncWarning { peer, path, reason } => {
            ("warning".to_owned(), format!("{peer} {path}: {reason}"))
        }
        WatchEvent::DeletesHeld {
            peer,
            space,
            mount,
            deletions,
            live,
        } => (
            "deletesHeld".to_owned(),
            format!("{peer} wants to delete {deletions} of {live} files in {space}/{mount}"),
        ),
        WatchEvent::Stopped => ("stop".to_owned(), "Sync loop stopped".to_owned()),
    }
}

fn record_error(app: &AppHandle, message: &str) {
    if let Some(state) = app.try_state::<crate::AppState>() {
        state.runner.set_state(
            app,
            RunnerState::Error {
                message: message.to_owned(),
            },
        );
        state.runner.push_event(
            app,
            ActivityItem {
                ts_ms: now_ms(),
                kind: "error".to_owned(),
                message: message.to_owned(),
            },
        );
    }
}

fn sleep_backoff(stop: &AtomicBool, backoff: Duration) -> bool {
    let deadline = std::time::Instant::now() + backoff;
    while std::time::Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    stop.load(Ordering::SeqCst)
}

fn join_with_timeout(handle: JoinHandle<()>, timeout: Duration) {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = handle.join();
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(timeout);
}

pub fn is_initialized(home: &Path) -> bool {
    match relay_engine::Engine::open_read_only(home) {
        Ok(_) => true,
        Err(relay_engine::EngineError::NotInitialized) => false,
        Err(_) => home.join("relay.db").is_file(),
    }
}

pub fn default_listen() -> SocketAddr {
    DEFAULT_LISTEN.parse().expect("default listen address")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        format!("sync thread panicked: {s}")
    } else if let Some(s) = payload.downcast_ref::<String>() {
        format!("sync thread panicked: {s}")
    } else {
        "sync thread panicked".to_owned()
    }
}

pub fn status_line(state: &RunnerState, connected: usize) -> String {
    match state {
        RunnerState::NotInitialized => "Relay — Not initialized".to_owned(),
        RunnerState::Starting => "Relay — Starting…".to_owned(),
        RunnerState::Running => {
            if connected == 1 {
                "Relay — Running, 1 peer online".to_owned()
            } else {
                format!("Relay — Running, {connected} peers online")
            }
        }
        RunnerState::Paused => "Relay — Paused".to_owned(),
        RunnerState::Error { .. } => "Relay — Error".to_owned(),
        RunnerState::ExternalService { .. } => "Relay — Syncing via background service".to_owned(),
    }
}
