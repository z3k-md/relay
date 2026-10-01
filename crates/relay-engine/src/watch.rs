//! Debounced filesystem watch loop. Events are hints; scans update the index.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use relay_core::{LogicalPath, MOUNT_MARKER, MountId, SpaceId};
use relay_fs::{MountWatcher, WatchSignal, to_logical_path};
use serde::Serialize;

use crate::Engine;
use crate::error::EngineError;
use crate::progress::TransferLive;
use crate::replica::ReplicaPush;
use crate::reports::{ScanOptions, ScanReport};
use crate::sync::{AddMountApplied, SyncEvent, SyncInput, SyncOutput, Syncer};

const STOP_POLL: Duration = Duration::from_millis(100);
const RELOAD_POLL: Duration = Duration::from_secs(1);
const REPLICA_PULL_INTERVAL: Duration = Duration::from_secs(5);

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
    /// Live transfer snapshot. Empty means nothing is in flight.
    Transfers(Vec<TransferLive>),
    ScanProgress {
        space: String,
        mount: String,
        files_seen: u64,
        bytes_hashed: u64,
    },
    Stopped,
}

struct MountWatch {
    space_id: SpaceId,
    mount_id: MountId,
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
        let mut last_replica_pull: Option<Instant>;
        let mut replica_warned = false;
        let mut syncer = Syncer::new();
        let (tx, rx) = mpsc::channel::<LoopMsg>();
        // Mount and share replies must not wait out a full index of a large
        // folder. Those inputs travel on their own channel so a scan can notice
        // them and pause.
        let (priority_tx, priority_rx) = mpsc::channel::<SyncInput>();
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                while let Ok(input) = sync_inputs.recv() {
                    match input {
                        input @ (SyncInput::AddMount { .. } | SyncInput::Share { .. }) => {
                            if priority_tx.send(input).is_err() {
                                break;
                            }
                        }
                        other => {
                            if tx.send(LoopMsg::Sync(other)).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
        let mut priority = PriorityQueue {
            rx: priority_rx,
            buf: VecDeque::new(),
        };

        let listed = self.mounts(None)?;
        let mut states: Vec<MountWatch> = listed
            .into_iter()
            .filter_map(|(space, config)| {
                let root = config.local_path?;
                Some(MountWatch {
                    space_id: space.id,
                    mount_id: config.mount.id,
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

        // Index inside the loop so an add-mount or share that arrives during
        // startup can be applied before a large tree finishes hashing.
        let due_at = Instant::now()
            .checked_sub(opts.debounce.saturating_add(Duration::from_millis(1)))
            .unwrap_or_else(Instant::now);
        for state in &mut states {
            state.full_pending = true;
            state.last_event = Some(due_at);
        }

        // Retry a mailbox push even when the scan found nothing. A crash after
        // the append and before the watermark leaves the entries in the mailbox
        // and the local cursor behind; the next edit would be the only retry.
        emit_push(
            self.push_replica_watch(),
            &mut replica_warned,
            on_event,
            &mut output,
        );
        emit_pull(self, &mut replica_warned, on_event, &mut output);
        last_replica_pull = Some(Instant::now());

        while !stop.load(Ordering::Relaxed) {
            // Apply queued mount/share commands before waiting on watcher input
            // or starting another index pass.
            let incoming = if let Some(input) = priority.pop() {
                Ok(LoopMsg::Sync(input))
            } else {
                rx.recv_timeout(STOP_POLL)
            };
            match incoming {
                Ok(LoopMsg::Fs(signal)) => apply_signal(&mut states, signal, Instant::now()),
                Ok(LoopMsg::Sync(input)) => match input {
                    SyncInput::Rescan { mounts } => {
                        mark_rescan(&mut states, &mounts, Instant::now());
                    }
                    SyncInput::AddPeer {
                        peer,
                        name,
                        addresses,
                        share,
                    } => {
                        if let Err(err) = apply_add_peer(
                            self,
                            &syncer,
                            peer,
                            &name,
                            &addresses,
                            &share,
                            &mut output,
                        ) {
                            on_event(&WatchEvent::SyncWarning {
                                peer: peer.to_string(),
                                path: String::new(),
                                reason: err.to_string(),
                            });
                        } else {
                            output(SyncOutput::SetPeers);
                        }
                    }
                    SyncInput::PeerAddresses { peer, addresses } => {
                        if let Err(err) = self.set_peer_addresses(peer, &addresses) {
                            on_event(&WatchEvent::SyncWarning {
                                peer: peer.to_string(),
                                path: String::new(),
                                reason: err.to_string(),
                            });
                        } else {
                            output(SyncOutput::SetPeers);
                        }
                    }
                    SyncInput::NatHint { addresses } => {
                        if let Err(err) = self.set_nat_hint(&addresses) {
                            on_event(&WatchEvent::SyncWarning {
                                peer: String::new(),
                                path: String::new(),
                                reason: err.to_string(),
                            });
                        } else {
                            match self.exchange_nat_detail() {
                                Ok(report) => {
                                    if report.addresses_changed {
                                        output(SyncOutput::SetPeers);
                                    }
                                    if let Some(addr) = report.relay_adopted {
                                        output(SyncOutput::SetRelay(Some(addr)));
                                    }
                                }
                                Err(err) => on_event(&WatchEvent::SyncWarning {
                                    peer: String::new(),
                                    path: String::new(),
                                    reason: err.to_string(),
                                }),
                            }
                        }
                    }
                    SyncInput::AddMount {
                        space,
                        mount,
                        path,
                        reply,
                    } => {
                        apply_add_mount(
                            self,
                            &mut syncer,
                            &mut states,
                            watcher.as_mut(),
                            &space,
                            &mount,
                            &path,
                            &mut output,
                            on_event,
                            reply,
                        );
                    }
                    SyncInput::Share { space, peer, reply } => {
                        let result = apply_share(self, &syncer, &space, &peer, &mut output);
                        let _ = reply.send(result);
                    }
                    other => {
                        emit_sync(syncer.handle(self, other, &mut output), on_event);
                        emit_sync(syncer.push_local_changes(self, &mut output), on_event);
                        emit_push(
                            self.push_replica_watch(),
                            &mut replica_warned,
                            on_event,
                            &mut output,
                        );
                    }
                },
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {}
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }

            let now = Instant::now();
            emit_sync(syncer.tick(self, now, &mut output), on_event);
            if let Some(rows) = syncer.poll_transfers(now) {
                on_event(&WatchEvent::Transfers(rows));
            }
            retry_watchers(
                watcher.as_mut(),
                &mut states,
                now,
                opts.full_scan_interval,
                on_event,
            );

            let jobs = flush_jobs(&mut states, now, &opts);
            let mut yielded_for_command = false;
            for job in jobs {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if priority.poll() {
                    yielded_for_command = true;
                    break;
                }
                let state = &mut states[job.index];
                let step = if job.full {
                    self.run_watch_scan(state, true, on_event, &mut || priority.poll())
                } else {
                    self.run_watch_scan_paths(state, &job.paths, on_event, &mut || priority.poll())
                };
                match step {
                    ScanStep::Yielded => {
                        yielded_for_command = true;
                        break;
                    }
                    ScanStep::Finished { committed } => {
                        if committed {
                            emit_sync(syncer.push_local_changes(self, &mut output), on_event);
                            emit_push(
                                self.push_replica_watch(),
                                &mut replica_warned,
                                on_event,
                                &mut output,
                            );
                        }
                    }
                }
            }
            if yielded_for_command {
                continue;
            }

            if last_replica_pull
                .is_none_or(|t| now.saturating_duration_since(t) >= REPLICA_PULL_INTERVAL)
            {
                last_replica_pull = Some(now);
                emit_pull(self, &mut replica_warned, on_event, &mut output);
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

    fn scan_reporting(
        &mut self,
        space: &str,
        mount: &str,
        paths: Option<&[LogicalPath]>,
        on_event: &mut dyn FnMut(&WatchEvent),
        poll_priority: &mut dyn FnMut() -> bool,
    ) -> Result<ScanReport, EngineError> {
        let mut files = 0u64;
        let mut bytes = 0u64;
        let mut last = Instant::now()
            .checked_sub(Duration::from_millis(250))
            .unwrap_or_else(Instant::now);
        let space_name = space.to_owned();
        let mount_name = mount.to_owned();
        let mut on_tick = |tick: crate::scan::ScanTick| {
            match tick {
                crate::scan::ScanTick::Visited => files += 1,
                crate::scan::ScanTick::Hashed(n) => bytes = bytes.saturating_add(n),
            }
            let now = Instant::now();
            if now.saturating_duration_since(last) >= Duration::from_millis(250)
                && (files > 0 || bytes > 0)
            {
                last = now;
                on_event(&WatchEvent::ScanProgress {
                    space: space_name.clone(),
                    mount: mount_name.clone(),
                    files_seen: files,
                    bytes_hashed: bytes,
                });
            }
            if poll_priority() {
                crate::scan::request_scan_yield();
            }
        };
        match paths {
            Some(paths) => {
                self.scan_paths_inner(space, mount, paths, ScanOptions::default(), &mut on_tick)
            }
            None => self.scan_mount(space, mount, ScanOptions::default(), &mut on_tick),
        }
    }

    fn run_watch_scan(
        &mut self,
        state: &mut MountWatch,
        full: bool,
        on_event: &mut dyn FnMut(&WatchEvent),
        poll_priority: &mut dyn FnMut() -> bool,
    ) -> ScanStep {
        let paths = state.dirty.len();
        let result = self.scan_reporting(&state.space, &state.mount, None, on_event, poll_priority);
        finish_or_yield(state, full, paths, result, on_event)
    }

    fn run_watch_scan_paths(
        &mut self,
        state: &mut MountWatch,
        paths: &[LogicalPath],
        on_event: &mut dyn FnMut(&WatchEvent),
        poll_priority: &mut dyn FnMut() -> bool,
    ) -> ScanStep {
        let n = paths.len();
        let result = self.scan_reporting(
            &state.space,
            &state.mount,
            Some(paths),
            on_event,
            poll_priority,
        );
        finish_or_yield(state, false, n, result, on_event)
    }
}

enum ScanStep {
    Finished { committed: bool },
    Yielded,
}

fn finish_or_yield(
    state: &mut MountWatch,
    full: bool,
    paths: usize,
    result: Result<ScanReport, EngineError>,
    on_event: &mut dyn FnMut(&WatchEvent),
) -> ScanStep {
    if matches!(result, Err(EngineError::Interrupted)) {
        // Leave the mount pending so the index resumes after the command.
        return ScanStep::Yielded;
    }
    let committed = result.as_ref().is_ok_and(scan_committed);
    finish_watch_scan(state, full, paths, result, on_event);
    ScanStep::Finished { committed }
}

fn apply_add_peer(
    engine: &mut Engine,
    syncer: &Syncer,
    peer: relay_core::DeviceId,
    name: &str,
    addresses: &[String],
    share: &[SpaceId],
    output: &mut dyn FnMut(SyncOutput),
) -> Result<(), EngineError> {
    engine.upsert_peer(name, peer, addresses)?;
    let mut shared = Vec::new();
    for space in share {
        match engine.share_space_id(*space, peer) {
            Ok(()) => shared.push(*space),
            Err(err) => {
                tracing::warn!(%peer, %space, error = %err, "could not share space with new peer");
            }
        }
    }
    for space in shared {
        if let Err(err) = syncer.refresh_offers_for_space(engine, space, output) {
            tracing::warn!(%space, error = %err, "could not refresh offers after add peer");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_add_mount(
    engine: &mut Engine,
    syncer: &mut Syncer,
    states: &mut Vec<MountWatch>,
    watcher: Option<&mut MountWatcher>,
    space: &str,
    mount: &str,
    path: &Path,
    output: &mut dyn FnMut(SyncOutput),
    on_event: &mut dyn FnMut(&WatchEvent),
    reply: mpsc::Sender<Result<AddMountApplied, String>>,
) {
    let config = match engine.add_mount(space, mount, path, &[], &[]) {
        Ok(config) => config,
        Err(err) => {
            let _ = reply.send(Err(err.to_string()));
            return;
        }
    };
    let space_id = config.mount.space;
    let mount_id = config.mount.id;
    let name = config.mount.name.clone();
    let local_path = config.local_path.clone();
    // Reply before watcher setup or UI events. Those run on this thread, and a
    // desktop command waiting on the reply can be the UI thread those events
    // need. The index itself runs later, off this reply.
    let _ = reply.send(Ok(AddMountApplied {
        name: name.clone(),
        path: local_path.clone(),
        space_id,
        mount_id,
    }));

    if let Some(root) = local_path {
        let mut state = MountWatch {
            space_id,
            mount_id,
            space: space.to_owned(),
            mount: name.clone(),
            root,
            dirty: HashSet::new(),
            first_event: None,
            last_event: Some(Instant::now()),
            full_pending: true,
            last_full: None,
            failed: false,
            watcher_attached: false,
        };
        if let Some(watcher) = watcher {
            attach_watcher(watcher, &mut state, on_event);
        }
        states.push(state);
        on_event(&WatchEvent::Started {
            mounts: vec![format!("{space}/{name}")],
        });
    }

    if let Err(err) = syncer.refresh_offers_for_space(engine, space_id, output) {
        on_event(&WatchEvent::SyncWarning {
            peer: String::new(),
            path: String::new(),
            reason: err.to_string(),
        });
    }
}

struct PriorityQueue {
    rx: mpsc::Receiver<SyncInput>,
    buf: VecDeque<SyncInput>,
}

impl PriorityQueue {
    /// Pull every waiting mount/share command into the buffer.
    /// True when at least one is waiting.
    fn poll(&mut self) -> bool {
        while let Ok(input) = self.rx.try_recv() {
            self.buf.push_back(input);
        }
        !self.buf.is_empty()
    }

    fn pop(&mut self) -> Option<SyncInput> {
        self.poll();
        self.buf.pop_front()
    }
}

fn apply_share(
    engine: &mut Engine,
    syncer: &Syncer,
    space: &str,
    peer: &str,
    output: &mut dyn FnMut(SyncOutput),
) -> Result<(), String> {
    engine.share(space, peer).map_err(|err| err.to_string())?;
    let space_id = engine
        .spaces()
        .map_err(|err| err.to_string())?
        .into_iter()
        .find(|s| s.name == space)
        .map(|s| s.id)
        .ok_or_else(|| format!("unknown space {space}"))?;
    syncer
        .refresh_offers_for_space(engine, space_id, output)
        .map_err(|err| err.to_string())?;
    Ok(())
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

fn emit_pull(
    engine: &mut Engine,
    warned: &mut bool,
    on_event: &mut dyn FnMut(&WatchEvent),
    output: &mut dyn FnMut(SyncOutput),
) {
    match engine.pull_replica_watch() {
        Ok(pull) => {
            if pull.addresses_changed {
                output(SyncOutput::SetPeers);
            }
            if let Some(addr) = pull.relay_adopted {
                output(SyncOutput::SetRelay(Some(addr)));
            }
        }
        Err(err) => warn_replica(err, warned, on_event),
    }
}

fn emit_push(
    result: Result<ReplicaPush, EngineError>,
    warned: &mut bool,
    on_event: &mut dyn FnMut(&WatchEvent),
    output: &mut dyn FnMut(SyncOutput),
) {
    match result {
        Ok(push) => {
            if let Some(addr) = push.relay_adopted {
                output(SyncOutput::SetRelay(Some(addr)));
            }
        }
        Err(err) => warn_replica(err, warned, on_event),
    }
}

fn warn_replica(err: EngineError, warned: &mut bool, on_event: &mut dyn FnMut(&WatchEvent)) {
    if !*warned {
        *warned = true;
        on_event(&WatchEvent::SyncWarning {
            peer: String::new(),
            path: String::new(),
            reason: format!("replica: {err}"),
        });
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
        SyncEvent::Transfers(rows) => WatchEvent::Transfers(rows.clone()),
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
            // Always emit, including a partial scan with no changes. A progress
            // tick may already have opened an index row, and the host clears
            // that row on Scanned. Callers hide the no-change partial from logs.
            on_event(&WatchEvent::Scanned {
                space: state.space.clone(),
                mount: state.mount.clone(),
                full,
                paths,
                report,
            });
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

fn mark_rescan(states: &mut [MountWatch], mounts: &[(SpaceId, MountId)], now: Instant) {
    for state in states {
        if mounts
            .iter()
            .any(|(space, mount)| *space == state.space_id && *mount == state.mount_id)
        {
            state.failed = false;
            state.full_pending = true;
            note_times(state, now);
            state.last_event = Some(now.checked_sub(Duration::from_secs(10)).unwrap_or(now));
        }
    }
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
