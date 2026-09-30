//! Debounced filesystem watch loop. Events are hints; scans update the index.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use relay_core::{LogicalPath, MOUNT_MARKER};
use relay_fs::{MountWatcher, WatchSignal, to_logical_path};
use serde::Serialize;

use crate::Engine;
use crate::error::EngineError;
use crate::reports::{ScanOptions, ScanReport};
use crate::sync::{SyncEvent, SyncInput, SyncOutput, Syncer};

const STOP_POLL: Duration = Duration::from_millis(100);
const RELOAD_POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchOptions {
    pub debounce: Duration,
    pub max_batch_delay: Duration,
    pub full_scan_interval: Duration,
    pub use_watcher: bool,
    pub max_dirty_paths: usize,
    /// When set, [`Engine::run`] polls SQLite `PRAGMA data_version` about once
    /// a second and returns [`RunExit::ExternalChange`] if another connection
    /// committed. Default `false` so `relay watch` and existing tests are
    /// unchanged.
    pub reload_on_external_change: bool,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(200),
            max_batch_delay: Duration::from_secs(2),
            full_scan_interval: Duration::from_secs(600),
            use_watcher: true,
            max_dirty_paths: 10_000,
            reload_on_external_change: false,
        }
    }
}

/// Why [`Engine::run`] returned successfully.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunExit {
    /// `stop` was set (or the input channel ended after a stop).
    Stopped,
    /// Another connection committed to the database (`PRAGMA data_version`).
    ExternalChange,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WatchEvent {
    Started {
        mounts: Vec<String>,
    },
    Scanned {
        space: String,
        mount: String,
        full: bool,
        paths: usize,
        report: ScanReport,
    },
    ScanFailed {
        space: String,
        mount: String,
        error: String,
    },
    WatcherUnavailable {
        space: String,
        mount: String,
        error: String,
    },
    PeerConnected {
        peer: String,
        name: String,
    },
    PeerDisconnected {
        peer: String,
    },
    OffersReceived {
        peer: String,
        spaces: Vec<String>,
    },
    RemoteApplied {
        peer: String,
        space: String,
        mount: String,
        written: usize,
        deleted: usize,
        conflicts: usize,
        skipped: usize,
    },
    SentChanges {
        peer: String,
        space: String,
        entries: usize,
    },
    SyncWarning {
        peer: String,
        path: String,
        reason: String,
    },
    DeletesHeld {
        peer: String,
        space: String,
        mount: String,
        deletions: usize,
        live: usize,
    },
    Stopped,
}

struct MountWatch {
    space: String,
    mount: String,
    root: PathBuf,
    dirty: HashSet<LogicalPath>,
    first_event: Option<Instant>,
    last_event: Option<Instant>,
    full_pending: bool,
    last_full: Option<Instant>,
    failed: bool,
    watcher_attached: bool,
}

enum LoopMsg {
    Fs(WatchSignal),
    Sync(SyncInput),
}

impl Engine {
    pub fn watch(
        &mut self,
        opts: WatchOptions,
        stop: &AtomicBool,
        on_event: &mut dyn FnMut(&WatchEvent),
    ) -> Result<(), EngineError> {
        let (_tx, rx) = mpsc::channel();
        match self.run(opts, rx, |_| {}, stop, on_event)? {
            RunExit::Stopped | RunExit::ExternalChange => Ok(()),
        }
    }

    /// Combined filesystem + peer sync loop.
    ///
    /// Performs an initial full scan of every local mount, then multiplexes
    /// watcher hints and [`SyncInput`]s on one channel. After every committing
    /// scan (or applied remote batch) new local changes are pushed to
    /// subscribed peers. `tick` is invoked on the poll interval so transient
    /// apply failures can retry.
    ///
    /// The daemon maps `relay-net`'s `NetEvent`/`NetCommand` 1:1 onto
    /// [`SyncInput`] / [`SyncOutput`].
    pub fn run(
        &mut self,
        opts: WatchOptions,
        sync_inputs: mpsc::Receiver<SyncInput>,
        mut output: impl FnMut(SyncOutput),
        stop: &AtomicBool,
        on_event: &mut dyn FnMut(&WatchEvent),
    ) -> Result<RunExit, EngineError> {
        self.ensure_writable()?;
        // Hold the run lock, then drop the writer lock so another process can
        // `Engine::open` and commit (peers, mounts, shares). SQLite WAL serializes
        // the actual writes; `data_version` on this connection sees theirs.
        let _run_lock = crate::acquire_run_lock(&self.home)?;
        self.release_writer_lock()?;
        let baseline = if opts.reload_on_external_change {
            Some(self.db.data_version()?)
        } else {
            None
        };
        let mut last_reload_check = Instant::now();
        let mut syncer = Syncer::new();
        let (tx, rx) = mpsc::channel::<LoopMsg>();
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                while let Ok(input) = sync_inputs.recv() {
                    if tx.send(LoopMsg::Sync(input)).is_err() {
                        break;
                    }
                }
            });
        }

        let listed = self.mounts(None)?;
        let mut states: Vec<MountWatch> = listed
            .into_iter()
            .filter_map(|(space, config)| {
                let root = config.local_path?;
                Some(MountWatch {
                    space: space.name,
                    mount: config.mount.name,
                    root,
                    dirty: HashSet::new(),
                    first_event: None,
                    last_event: None,
                    full_pending: false,
                    last_full: None,
                    failed: false,
                    watcher_attached: false,
                })
            })
            .collect();

        let (fs_tx, fs_rx) = mpsc::channel();
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                while let Ok(signal) = fs_rx.recv() {
                    if tx.send(LoopMsg::Fs(signal)).is_err() {
                        break;
                    }
                }
            });
        }
        let mut watcher = if opts.use_watcher {
            match MountWatcher::new(fs_tx.clone()) {
                Ok(w) => Some(w),
                Err(err) => {
                    let error = err.to_string();
                    for state in &states {
                        on_event(&WatchEvent::WatcherUnavailable {
                            space: state.space.clone(),
                            mount: state.mount.clone(),
                            error: error.clone(),
                        });
                    }
                    None
                }
            }
        } else {
            None
        };
        let _keep_open = tx;

        if let Some(watcher) = watcher.as_mut() {
            for state in &mut states {
                attach_watcher(watcher, state, on_event);
            }
        }

        let names: Vec<String> = states
            .iter()
            .map(|s| format!("{}/{}", s.space, s.mount))
            .collect();
        on_event(&WatchEvent::Started { mounts: names });

        let initial: Vec<(String, String)> = states
            .iter()
            .map(|s| (s.space.clone(), s.mount.clone()))
            .collect();
        for (space, mount) in initial {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let result = self.scan(&space, &mount, ScanOptions::default());
            let committed = result.as_ref().is_ok_and(scan_committed);
            if let Some(state) = states
                .iter_mut()
                .find(|s| s.space == space && s.mount == mount)
            {
                finish_watch_scan(state, true, 0, result, on_event);
            }
            if committed {
                emit_sync(syncer.push_local_changes(self, &mut output), on_event);
            }
        }

        while !stop.load(Ordering::Relaxed) {
            match rx.recv_timeout(STOP_POLL) {
                Ok(LoopMsg::Fs(signal)) => apply_signal(&mut states, signal, Instant::now()),
                Ok(LoopMsg::Sync(input)) => {
                    emit_sync(syncer.handle(self, input, &mut output), on_event);
                    emit_sync(syncer.push_local_changes(self, &mut output), on_event);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {}
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }

            let now = Instant::now();
            emit_sync(syncer.tick(self, now, &mut output), on_event);
            retry_watchers(
                watcher.as_mut(),
                &mut states,
                now,
                opts.full_scan_interval,
                on_event,
            );

            let jobs = flush_jobs(&mut states, now, &opts);
            for job in jobs {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let state = &mut states[job.index];
                let committed = if job.full {
                    self.run_watch_scan(state, true, on_event)
                } else {
                    self.run_watch_scan_paths(state, &job.paths, on_event)
                };
                if committed {
                    emit_sync(syncer.push_local_changes(self, &mut output), on_event);
                }
            }

            if let Some(baseline) = baseline
                && now.saturating_duration_since(last_reload_check) >= RELOAD_POLL
            {
                last_reload_check = now;
                if self.db.data_version()? != baseline {
                    drop(watcher);
                    self.try_reacquire_writer_lock()?;
                    return Ok(RunExit::ExternalChange);
                }
            }
        }

        drop(watcher);
        on_event(&WatchEvent::Stopped);
        self.try_reacquire_writer_lock()?;
        Ok(RunExit::Stopped)
    }

    fn run_watch_scan(
        &mut self,
        state: &mut MountWatch,
        full: bool,
        on_event: &mut dyn FnMut(&WatchEvent),
    ) -> bool {
        let paths = state.dirty.len();
        let result = self.scan(&state.space, &state.mount, ScanOptions::default());
        let committed = result.as_ref().is_ok_and(scan_committed);
        finish_watch_scan(state, full, paths, result, on_event);
        committed
    }

    fn run_watch_scan_paths(
        &mut self,
        state: &mut MountWatch,
        paths: &[LogicalPath],
        on_event: &mut dyn FnMut(&WatchEvent),
    ) -> bool {
        let n = paths.len();
        let result = self.scan_paths(&state.space, &state.mount, paths, ScanOptions::default());
        let committed = result.as_ref().is_ok_and(scan_committed);
        finish_watch_scan(state, false, n, result, on_event);
        committed
    }
}

fn scan_committed(report: &ScanReport) -> bool {
    report.created > 0 || report.modified > 0 || report.deleted > 0
}

fn emit_sync(result: Result<Vec<SyncEvent>, EngineError>, on_event: &mut dyn FnMut(&WatchEvent)) {
    match result {
        Ok(events) => {
            for event in events {
                on_event(&watch_from_sync(&event));
            }
        }
        Err(err) => on_event(&WatchEvent::SyncWarning {
            peer: String::new(),
            path: String::new(),
            reason: err.to_string(),
        }),
    }
}

fn watch_from_sync(event: &SyncEvent) -> WatchEvent {
    match event {
        SyncEvent::PeerConnected { peer, name } => WatchEvent::PeerConnected {
            peer: peer.to_string(),
            name: name.clone(),
        },
        SyncEvent::PeerDisconnected { peer } => WatchEvent::PeerDisconnected {
            peer: peer.to_string(),
        },
        SyncEvent::OffersReceived { peer, spaces } => WatchEvent::OffersReceived {
            peer: peer.to_string(),
            spaces: spaces.iter().map(|s| s.name.clone()).collect(),
        },
        SyncEvent::RemoteApplied {
            peer,
            space,
            mount,
            written,
            deleted,
            conflicts,
            skipped,
        } => WatchEvent::RemoteApplied {
            peer: peer.to_string(),
            space: space.clone(),
            mount: mount.clone(),
            written: *written,
            deleted: *deleted,
            conflicts: *conflicts,
            skipped: *skipped,
        },
        SyncEvent::SentChanges {
            peer,
            space,
            entries,
        } => WatchEvent::SentChanges {
            peer: peer.to_string(),
            space: space.clone(),
            entries: *entries,
        },
        SyncEvent::SyncWarning { peer, path, reason } => WatchEvent::SyncWarning {
            peer: peer.to_string(),
            path: path.clone(),
            reason: reason.clone(),
        },
        SyncEvent::DeletesHeld {
            peer,
            space,
            mount,
            deletions,
            live,
        } => WatchEvent::DeletesHeld {
            peer: peer.to_string(),
            space: space.clone(),
            mount: mount.clone(),
            deletions: *deletions,
            live: *live,
        },
    }
}

struct FlushJob {
    index: usize,
    full: bool,
    paths: Vec<LogicalPath>,
}

fn finish_watch_scan(
    state: &mut MountWatch,
    full: bool,
    paths: usize,
    result: Result<ScanReport, EngineError>,
    on_event: &mut dyn FnMut(&WatchEvent),
) {
    match result {
        Ok(report) => {
            state.dirty.clear();
            state.first_event = None;
            state.last_event = None;
            state.full_pending = false;
            state.failed = false;
            if full {
                state.last_full = Some(Instant::now());
            }
            if full || report.has_changes() {
                on_event(&WatchEvent::Scanned {
                    space: state.space.clone(),
                    mount: state.mount.clone(),
                    full,
                    paths,
                    report,
                });
            }
        }
        Err(err) => {
            state.failed = true;
            if full {
                state.last_full = Some(Instant::now());
            }
            on_event(&WatchEvent::ScanFailed {
                space: state.space.clone(),
                mount: state.mount.clone(),
                error: err.to_string(),
            });
        }
    }
}

fn attach_watcher(
    watcher: &mut MountWatcher,
    state: &mut MountWatch,
    on_event: &mut dyn FnMut(&WatchEvent),
) {
    match watcher.watch(&state.root) {
        Ok(()) => state.watcher_attached = true,
        Err(err) => {
            state.watcher_attached = false;
            on_event(&WatchEvent::WatcherUnavailable {
                space: state.space.clone(),
                mount: state.mount.clone(),
                error: err.to_string(),
            });
        }
    }
}

fn retry_watchers(
    mut watcher: Option<&mut MountWatcher>,
    states: &mut [MountWatch],
    now: Instant,
    interval: Duration,
    on_event: &mut dyn FnMut(&WatchEvent),
) {
    for state in states.iter_mut() {
        let periodic = state
            .last_full
            .is_some_and(|t| now.saturating_duration_since(t) >= interval);
        if !periodic {
            continue;
        }
        if !state.watcher_attached
            && let Some(watcher) = watcher.as_deref_mut()
        {
            attach_watcher(watcher, state, on_event);
        }
        state.failed = false;
        state.full_pending = true;
        note_times(state, now);
    }
}

fn apply_signal(states: &mut [MountWatch], signal: WatchSignal, now: Instant) {
    match signal {
        WatchSignal::Changed { root, paths } => {
            let Some(state) = find_mount(states, &root) else {
                return;
            };
            if paths.iter().any(|p| forces_full_scan(&state.root, p)) {
                state.full_pending = true;
                note_event(state, now);
                return;
            }
            for os_path in paths {
                match to_logical_path(&state.root, &os_path) {
                    Ok(logical) => {
                        state.dirty.insert(logical);
                    }
                    Err(_) => {
                        state.full_pending = true;
                    }
                }
            }
            note_event(state, now);
        }
        WatchSignal::Rescan { root, .. } => {
            let Some(state) = find_mount(states, &root) else {
                return;
            };
            state.full_pending = true;
            note_event(state, now);
        }
    }
}

fn forces_full_scan(root: &Path, os_path: &Path) -> bool {
    if os_path == root.join(".relayignore") || os_path == root.join(MOUNT_MARKER) {
        return true;
    }
    match to_logical_path(root, os_path) {
        Ok(path) => path.as_str() == ".relayignore" || path.as_str() == MOUNT_MARKER,
        Err(_) => false,
    }
}

fn find_mount<'a>(states: &'a mut [MountWatch], root: &Path) -> Option<&'a mut MountWatch> {
    if let Some(i) = states.iter().position(|s| s.root == root) {
        return Some(&mut states[i]);
    }
    let canon = dunce::canonicalize(root).ok();
    states.iter_mut().find(|s| {
        if let Some(c) = &canon
            && let Ok(other) = dunce::canonicalize(&s.root)
        {
            return *c == other;
        }
        s.root == root
    })
}

fn note_event(state: &mut MountWatch, now: Instant) {
    state.failed = false;
    note_times(state, now);
}

fn note_times(state: &mut MountWatch, now: Instant) {
    if state.first_event.is_none() {
        state.first_event = Some(now);
    }
    state.last_event = Some(now);
}

fn flush_jobs(states: &mut [MountWatch], now: Instant, opts: &WatchOptions) -> Vec<FlushJob> {
    let mut jobs = Vec::new();
    for (index, state) in states.iter_mut().enumerate() {
        if state.failed {
            continue;
        }
        let periodic_full = state.full_pending
            && state
                .last_full
                .is_some_and(|t| now.saturating_duration_since(t) >= opts.full_scan_interval);
        if !periodic_full && !due_for_flush(state, now, opts) {
            continue;
        }
        if !state.full_pending && state.dirty.is_empty() {
            continue;
        }
        let full = state.full_pending || state.dirty.len() > opts.max_dirty_paths;
        let paths = if full {
            Vec::new()
        } else {
            state.dirty.iter().cloned().collect()
        };
        jobs.push(FlushJob { index, full, paths });
    }
    jobs
}

fn due_for_flush(state: &MountWatch, now: Instant, opts: &WatchOptions) -> bool {
    let Some(last) = state.last_event else {
        return false;
    };
    now.saturating_duration_since(last) >= opts.debounce
        || state
            .first_event
            .is_some_and(|first| now.saturating_duration_since(first) >= opts.max_batch_delay)
}
