use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use relay_engine::{
    TransferDirection, TransferLive, WatchEvent, WatchOptions, bookends, index_row, summary_line,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

#[cfg(not(target_os = "android"))]
use crate::sidecar;
#[cfg(not(target_os = "android"))]
use crate::tray;
use relay_daemon::{DaemonEvent, DaemonOptions, HostKind};

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
    // Stopped on purpose: an update is installing, or the app is quitting.
    Stopped,
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
    /// Peer id → unix ms when the current session started.
    connected: HashMap<String, i64>,
    /// Tells the current sync thread to stop. Fresh for every start, so a
    /// slow old loop never sees its own flag lowered again.
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    transfers: Vec<TransferLive>,
    scans: Vec<TransferLive>,
    published: Vec<TransferLive>,
}

pub struct Runner {
    home: PathBuf,
    inner: Mutex<Inner>,
    /// Held for a whole start or stop. Commands run on worker threads, so
    /// without it two starts could both find no thread and spawn two loops.
    lifecycle: Mutex<()>,
}

impl Runner {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            inner: Mutex::new(Inner {
                state: RunnerState::NotInitialized,
                events: VecDeque::new(),
                connected: HashMap::new(),
                stop: Arc::new(AtomicBool::new(false)),
                thread: None,
                transfers: Vec::new(),
                scans: Vec::new(),
                published: Vec::new(),
            }),
            lifecycle: Mutex::new(()),
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
            .map(|g| g.connected.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn connected_since(&self) -> HashMap<String, i64> {
        self.inner
            .lock()
            .map(|g| g.connected.clone())
            .unwrap_or_default()
    }

    pub fn connected_count(&self) -> usize {
        self.inner.lock().map(|g| g.connected.len()).unwrap_or(0)
    }

    pub fn start(&self, app: &AppHandle) {
        let _lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        self.start_locked(app);
    }

    fn start_locked(&self, app: &AppHandle) {
        if external_service_running(&self.home) {
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
        self.stop_join_locked(app);
        self.set_state(app, RunnerState::Starting);

        let home = self.home.clone();
        let stop = Arc::new(AtomicBool::new(false));
        if let Ok(mut inner) = self.inner.lock() {
            inner.stop = Arc::clone(&stop);
        }
        let app = app.clone();
        let handle = thread::Builder::new()
            .name("relay-sync".to_owned())
            .spawn(move || sync_loop(home, stop, app))
            .expect("spawn sync thread");

        if let Ok(mut inner) = self.inner.lock() {
            inner.thread = Some(handle);
        }
    }

    /// Stop the sync thread and wait (briefly) for it. Shows as Stopped with
    /// no peers or transfers until the next start, so the UI never says
    /// Running while nothing syncs.
    pub fn stop_join(&self, app: &AppHandle) {
        let _lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        self.stop_join_locked(app);
    }

    fn stop_join_locked(&self, app: &AppHandle) {
        let handle = self.inner.lock().ok().and_then(|mut g| {
            g.stop.store(true, Ordering::SeqCst);
            g.thread.take()
        });
        let Some(handle) = handle else {
            return;
        };
        join_with_timeout(handle, JOIN_TIMEOUT);
        if let Ok(mut inner) = self.inner.lock() {
            inner.connected.clear();
            inner.transfers.clear();
            inner.scans.clear();
            inner.published.clear();
        }
        self.set_state(app, RunnerState::Stopped);
        let _ = app.emit("relay://transfers", &Vec::<UiTransfer>::new());
    }

    pub fn restart(&self, app: &AppHandle) {
        let _lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        self.stop_join_locked(app);
        self.start_locked(app);
    }

    pub fn pause(&self, _app: &AppHandle) -> anyhow::Result<()> {
        if matches!(self.state(), RunnerState::ExternalService { .. }) {
            anyhow::bail!("{EXTERNAL_SERVICE_MESSAGE}");
        }
        let mut engine = relay_engine::Engine::open_for_config(&self.home)?;
        engine.set_paused(true)?;
        Ok(())
    }

    pub fn resume(&self, app: &AppHandle) -> anyhow::Result<()> {
        if matches!(self.state(), RunnerState::ExternalService { .. }) {
            anyhow::bail!("{EXTERNAL_SERVICE_MESSAGE}");
        }
        let mut engine = relay_engine::Engine::open_for_config(&self.home)?;
        engine.set_paused(false)?;
        let _lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(
            self.state(),
            RunnerState::Starting | RunnerState::Running | RunnerState::Paused
        ) {
            self.start_locked(app);
        }
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
        refresh_tray(app);
    }

    fn push_event(&self, app: &AppHandle, item: ActivityItem) {
        if let Ok(mut inner) = self.inner.lock() {
            match item.kind.as_str() {
                "peerConnected" => {
                    if let Some(id) = item.message.split_whitespace().last() {
                        inner.connected.entry(id.to_owned()).or_insert_with(now_ms);
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
        refresh_tray(app);
    }

    fn apply_progress(&self, app: &AppHandle, event: &WatchEvent) {
        let update = {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            let changed = match event {
                WatchEvent::Transfers(rows) => {
                    inner.transfers = rows.clone();
                    true
                }
                WatchEvent::ScanProgress {
                    space,
                    mount,
                    files_seen,
                    bytes_hashed,
                } => {
                    if let Some(row) = inner
                        .scans
                        .iter_mut()
                        .find(|row| row.space == *space && row.mount.as_deref() == Some(mount))
                    {
                        row.files_done = *files_seen;
                        row.bytes_done = *bytes_hashed;
                    } else {
                        let mut row = index_row(space, mount, *files_seen, *bytes_hashed);
                        row.started_at_ms = now_ms().max(0) as u64;
                        inner.scans.push(row);
                    }
                    true
                }
                WatchEvent::Scanned { space, mount, .. }
                | WatchEvent::ScanFailed { space, mount, .. } => {
                    let before = inner.scans.len();
                    inner.scans.retain(|row| {
                        !(row.space == *space && row.mount.as_deref() == Some(mount))
                    });
                    before != inner.scans.len()
                }
                _ => false,
            };
            if !changed {
                return;
            }
            let mut next = inner.transfers.clone();
            next.extend(inner.scans.iter().cloned());
            let lines = bookends(&inner.published, &next, now_ms().max(0) as u64);
            inner.published = next.clone();
            (next, lines)
        };
        for line in update.1 {
            self.push_event(
                app,
                ActivityItem {
                    ts_ms: now_ms(),
                    kind: line.kind,
                    message: line.summary,
                },
            );
        }
        let _ = app.emit("relay://transfers", &ui_transfers(&update.0));
        refresh_tray(app);
    }

    pub fn transfer_summary(&self) -> Option<String> {
        let inner = self.inner.lock().ok()?;
        if matches!(inner.state, RunnerState::Paused) {
            return None;
        }
        summary_line(&inner.published)
    }

    fn apply_watch_peer(&self, event: &WatchEvent) {
        if let Ok(mut inner) = self.inner.lock() {
            match event {
                WatchEvent::PeerConnected { peer, .. } => {
                    inner.connected.entry(peer.clone()).or_insert_with(now_ms);
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
        if external_service_running(&home) {
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
            host: HostKind::Desktop,
            enable_stun: true,
            loopback_only: false,
            placeholders: true,
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
        runner.apply_progress(app, watch);
    }
    match event {
        DaemonEvent::Started { .. } => runner.set_state(app, RunnerState::Running),
        DaemonEvent::Paused => runner.set_state(app, RunnerState::Paused),
        DaemonEvent::Resumed => runner.set_state(app, RunnerState::Starting),
        DaemonEvent::Reloading => {}
        _ => {}
    }
    if hide_from_activity(event) {
        return;
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

fn hide_from_activity(event: &DaemonEvent) -> bool {
    match event {
        DaemonEvent::Watch(WatchEvent::Transfers(_) | WatchEvent::ScanProgress { .. }) => true,
        DaemonEvent::Watch(WatchEvent::Scanned { full, report, .. }) => {
            !*full && !report.has_changes()
        }
        _ => false,
    }
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
        DaemonEvent::Paused => ("paused".to_owned(), "Paused".to_owned()),
        DaemonEvent::Resumed => ("resumed".to_owned(), "Resumed".to_owned()),
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
        WatchEvent::MountRemoved { space, mount } => (
            "watch".to_owned(),
            format!("Stopped syncing {space}/{mount}"),
        ),
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
        WatchEvent::Transfers(rows) => (
            "transfers".to_owned(),
            if rows.is_empty() {
                "Transfers idle".to_owned()
            } else {
                format!("{} transfer(s) in flight", rows.len())
            },
        ),
        WatchEvent::ScanProgress {
            space,
            mount,
            files_seen,
            bytes_hashed,
        } => (
            "scanProgress".to_owned(),
            format!("{space}/{mount}: indexed {files_seen} files, {bytes_hashed} bytes"),
        ),
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UiTransfer {
    peer_id: String,
    peer_name: String,
    space: String,
    mount: Option<String>,
    direction: String,
    files_done: u64,
    files_total: Option<u64>,
    bytes_done: u64,
    bytes_total: Option<u64>,
    bytes_per_sec: u64,
    started_at_ms: u64,
    retries: u64,
    current_path: Option<String>,
}

fn ui_transfers(rows: &[TransferLive]) -> Vec<UiTransfer> {
    rows.iter()
        .map(|row| UiTransfer {
            peer_id: row.peer_id.clone(),
            peer_name: row.peer_name.clone(),
            space: row.space.clone(),
            mount: row.mount.clone(),
            direction: match row.direction {
                TransferDirection::Receive => "receive",
                TransferDirection::Send => "send",
                TransferDirection::Index => "index",
            }
            .to_owned(),
            files_done: row.files_done,
            files_total: row.files_total,
            bytes_done: row.bytes_done,
            bytes_total: row.bytes_total,
            started_at_ms: row.started_at_ms,
            bytes_per_sec: row.bytes_per_sec,
            retries: row.retries,
            current_path: row.current_path.clone(),
        })
        .collect()
}

fn external_service_running(home: &Path) -> bool {
    #[cfg(not(target_os = "android"))]
    {
        sidecar::service_is_running(home)
    }
    #[cfg(target_os = "android")]
    {
        let _ = home;
        false
    }
}

fn refresh_tray(app: &AppHandle) {
    #[cfg(not(target_os = "android"))]
    tray::refresh(app);
    #[cfg(target_os = "android")]
    let _ = app;
}

#[cfg(not(target_os = "android"))]
pub fn status_line(state: &RunnerState, connected: usize, summary: Option<&str>) -> String {
    match state {
        RunnerState::NotInitialized => "Relay — Not initialized".to_owned(),
        RunnerState::Starting => "Relay — Starting…".to_owned(),
        RunnerState::Running => {
            if let Some(summary) = summary {
                format!("Relay — {summary}")
            } else if connected == 1 {
                "Relay — Running, 1 peer online".to_owned()
            } else {
                format!("Relay — Running, {connected} peers online")
            }
        }
        RunnerState::Paused => "Relay — Paused".to_owned(),
        RunnerState::Stopped => "Relay — Stopped".to_owned(),
        RunnerState::Error { .. } => "Relay — Error".to_owned(),
        RunnerState::ExternalService { .. } => "Relay — Syncing via background service".to_owned(),
    }
}
