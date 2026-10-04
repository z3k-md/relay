//! Debounced filesystem watch loop. Events are hints; scans update the index.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use relay_core::{ConfigApplied, ConfigChange, LogicalPath, MOUNT_MARKER, Mount, MountId, SpaceId};
use relay_fs::{MountWatcher, WatchSignal, to_logical_path};
use serde::Serialize;

use crate::Engine;
use crate::error::EngineError;
use crate::live_config::{Applied, ConfigQueue};
use crate::progress::TransferLive;
use crate::replica::ReplicaPush;
use crate::reports::{ScanOptions, ScanReport};
use crate::sync::{SyncEvent, SyncInput, SyncOutput, Syncer};

mod events;
mod mounts;
mod scans;

use events::{emit_pull, emit_push, emit_sync};
use mounts::{after_config, apply_signal, attach_watcher, flush_jobs, mark_rescan, retry_watchers};
use scans::ScanStep;

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
    /// A mount was detached live and is no longer watched or scanned.
    MountRemoved {
        space: String,
        mount: String,
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
        let mut config = ConfigQueue::default();
        let (tx, rx) = mpsc::channel::<LoopMsg>();
        // Config and fetch replies must not wait out a full index of a large
        // folder. Those inputs travel on their own channel so a scan can notice
        // them and pause.
        let (priority_tx, priority_rx) = mpsc::channel::<SyncInput>();
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                while let Ok(input) = sync_inputs.recv() {
                    match input {
                        input @ (SyncInput::Config { .. } | SyncInput::Fetch { .. }) => {
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
                    SyncInput::Config { change, reply } => {
                        if let Some(applied) = config.submit(self, change, reply) {
                            after_config(
                                self,
                                &mut syncer,
                                &mut states,
                                watcher.as_mut(),
                                &applied,
                                &mut output,
                                on_event,
                            );
                        }
                    }
                    other => {
                        // Only a peer frame can carry the offer a waiting join needs.
                        let may_bring_offer = matches!(other, SyncInput::Frame { .. });
                        emit_sync(syncer.handle(self, other, &mut output), on_event);
                        if may_bring_offer && config.is_waiting() {
                            for applied in config.retry(self) {
                                after_config(
                                    self,
                                    &mut syncer,
                                    &mut states,
                                    watcher.as_mut(),
                                    &applied,
                                    &mut output,
                                    on_event,
                                );
                            }
                        }
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
            config.expire(now);
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
