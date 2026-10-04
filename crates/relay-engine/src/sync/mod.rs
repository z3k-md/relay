//! Engine-native peer sync I/O. The CLI/daemon maps these 1:1 onto relay-net.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use relay_core::version::VectorOrdering;
use relay_core::{
    ConfigApplied, ConfigChange, DeleteHoldDecision, DeviceId, EntryContent, EntryKey, MountId,
    ObjectId, Sequence, SpaceId,
};
use relay_db::{OfferedMember, PeerOfferRow};
use relay_proto::{
    Ack, INDEX_BATCH_ENTRIES, IndexBatch, IndexRequest, RemoteEntry, device_id_from_bytes,
    entry_from_wire, entry_to_wire, frame, space_id_bytes, space_id_from_bytes,
};
use serde::Serialize;

use crate::Engine;
use crate::error::EngineError;
use crate::live_config::ConfigRejected;
use crate::materialize::{FetchPrep, MaterializationMode, path_mode};
use crate::peers::offered_mounts_from_wire;
use crate::progress::{IncomingFile, ProgressBook, TransferLive};
use crate::replica::open_replica;
use crate::secrets::MailboxRead;

mod deletes;
mod fetch;
mod index;
mod offers;

use deletes::{MassDeleteAction, applied_tombstone_paths, live_tombstones};
use index::{index_request, resume_index};

const RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_ATTEMPTS: u32 = 3;
/// Fetches of one object that failed for a reason other than "not found"
/// before the entries needing it are left for a later re-request.
const MAX_FETCH_ATTEMPTS: u32 = 3;
/// Delay before re-requesting a range whose objects could not be fetched.
const RESYNC_DELAY: Duration = Duration::from_secs(30);
/// How often a live session refreshes `devices.last_seen_ms`. A crash between
/// refreshes still leaves the offline duration within this window.
const SEEN_INTERVAL: Duration = Duration::from_secs(30);
/// How often index-only rows are checked for a mode that now wants bytes.
/// The watch loop ticks much faster than this; a metadata tree must not be
/// walked on every poll.
const HYDRATE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub enum SyncInput {
    PeerConnected {
        peer: DeviceId,
        name: String,
    },
    PeerDisconnected {
        peer: DeviceId,
    },
    Frame {
        peer: DeviceId,
        body: frame::Body,
    },
    ObjectFetched {
        peer: DeviceId,
        object: ObjectId,
    },
    ObjectFetchFailed {
        peer: DeviceId,
        object: ObjectId,
        not_found: bool,
        reason: String,
    },
    /// Absolute bytes of `object` received (`incoming`) or served so far.
    ObjectProgress {
        peer: DeviceId,
        object: ObjectId,
        incoming: bool,
        bytes: u64,
    },
    Rescan {
        mounts: Vec<(SpaceId, MountId)>,
    },
    AddPeer {
        peer: DeviceId,
        name: String,
        addresses: Vec<String>,
        share: Vec<SpaceId>,
    },
    PeerAddresses {
        peer: DeviceId,
        addresses: Vec<String>,
    },
    /// Addresses to publish as this device's NAT candidates (LAN and reflexive).
    NatHint {
        addresses: Vec<String>,
    },
    /// Apply a config change through the loop writer (D19 / D24 / D25), so
    /// live sessions survive. The reply is sent before any watcher or UI
    /// follow-up runs.
    Config {
        change: ConfigChange,
        reply: mpsc::Sender<Result<ConfigApplied, ConfigRejected>>,
    },
    /// Hydrate one demand-mode path, asking a connected peer when needed.
    Fetch {
        space: String,
        mount: String,
        path: String,
        reply: mpsc::Sender<Result<(), String>>,
    },
}

#[derive(Clone, Debug)]
pub enum SyncOutput {
    Send {
        peer: DeviceId,
        body: frame::Body,
    },
    FetchObject {
        peer: DeviceId,
        object: ObjectId,
    },
    SetPeers,
    /// Relay address learned from the mailbox. Applied without reloading.
    SetRelay(Option<String>),
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SyncEvent {
    PeerConnected {
        peer: DeviceId,
        name: String,
    },
    PeerDisconnected {
        peer: DeviceId,
    },
    OffersReceived {
        peer: DeviceId,
        spaces: Vec<OfferedSpaceEvent>,
    },
    RemoteApplied {
        peer: DeviceId,
        space: String,
        mount: String,
        written: usize,
        deleted: usize,
        conflicts: usize,
        skipped: usize,
    },
    SentChanges {
        peer: DeviceId,
        space: String,
        entries: usize,
    },
    SyncWarning {
        peer: DeviceId,
        path: String,
        reason: String,
    },
    DeletesHeld {
        peer: DeviceId,
        space: String,
        mount: String,
        deletions: usize,
        live: usize,
    },
    Transfers(Vec<TransferLive>),
}

#[derive(Clone, Debug, Serialize)]
pub struct OfferedSpaceEvent {
    pub name: String,
    pub id: SpaceId,
    pub already_joined: bool,
}

struct Connected {
    name: String,
    send_cursor: HashMap<SpaceId, Sequence>,
    incoming: HashMap<SpaceId, VecDeque<PendingBatch>>,
    /// Highest peer sequence below which some entry could not be applied
    /// because its object never arrived. `received_seq` is not advanced past
    /// it, so a restart or re-request picks those entries up again.
    holes: HashMap<SpaceId, u64>,
    resync_at: HashMap<SpaceId, Instant>,
    /// Live-local tombstones already applied for this catch-up, per mount.
    session_deletes: HashMap<(SpaceId, MountId), usize>,
    /// Paths whose local entry became a tombstone from those applies.
    session_deleted_paths: HashMap<(SpaceId, MountId), Vec<relay_core::LogicalPath>>,
    /// `DeletesHeld` already emitted for this connection.
    held_emitted: HashSet<(SpaceId, MountId)>,
}

#[derive(Clone, Copy)]
struct QueuedBatch {
    peer: DeviceId,
    space: SpaceId,
    id: u64,
}

struct PendingBatch {
    id: u64,
    after_sequence: u64,
    through_sequence: u64,
    entries: Vec<RemoteEntry>,
    pending_objects: HashSet<ObjectId>,
    /// Peers already asked for each object, including the index source.
    asked: HashMap<ObjectId, HashSet<DeviceId>>,
    /// Index source reported the object missing. Alternate peers do not set this.
    source_missing: HashSet<ObjectId>,
    failed_objects: HashSet<ObjectId>,
    fetch_attempts: HashMap<ObjectId, u32>,
    attempts: u32,
    retry_at: Option<Instant>,
    caught_up: bool,
}

#[derive(Default)]
struct DirectFetch {
    space: Option<SpaceId>,
    attempts: u32,
    asked: HashSet<DeviceId>,
    waiting: bool,
    gave_up: bool,
    warned: bool,
}

struct FetchWaiter {
    key: EntryKey,
    reply: mpsc::Sender<Result<(), String>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fetch {
    Ok,
    Failed { not_found: bool },
}

pub struct Syncer {
    connected: HashMap<DeviceId, Connected>,
    /// Last time this process wrote `last_seen_ms` for a live peer.
    seen_at: HashMap<DeviceId, Instant>,
    index_batch_entries: usize,
    progress: ProgressBook,
    next_batch_id: u64,
    /// Objects requested for full-mode hydration or an explicit fetch.
    direct: HashMap<ObjectId, DirectFetch>,
    fetch_waiters: HashMap<ObjectId, Vec<FetchWaiter>>,
    /// Non-file hydration failures already reported.
    hydrate_warned: HashSet<EntryKey>,
    /// Last time index-only rows were considered for hydration.
    hydrated_at: Option<Instant>,
}

impl Default for Syncer {
    fn default() -> Self {
        Self {
            connected: HashMap::new(),
            seen_at: HashMap::new(),
            index_batch_entries: INDEX_BATCH_ENTRIES,
            progress: ProgressBook::default(),
            next_batch_id: 0,
            direct: HashMap::new(),
            fetch_waiters: HashMap::new(),
            hydrate_warned: HashSet::new(),
            hydrated_at: None,
        }
    }
}

impl Syncer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Send index batches of at most `n` entries instead of
    /// `INDEX_BATCH_ENTRIES`, so tests can cover multi-batch exchanges with
    /// few files.
    pub fn with_index_batch_entries(n: usize) -> Self {
        assert!(n > 0, "index batches need at least one entry");
        Self {
            index_batch_entries: n,
            ..Self::default()
        }
    }

    pub fn handle(
        &mut self,
        engine: &mut Engine,
        input: SyncInput,
        out: &mut dyn FnMut(SyncOutput),
    ) -> Result<Vec<SyncEvent>, EngineError> {
        let mut events = Vec::new();
        match input {
            SyncInput::PeerConnected { peer, name } => {
                self.on_connected(engine, peer, name, out, &mut events)?;
            }
            SyncInput::PeerDisconnected { peer } => {
                let was_connected = self.connected.contains_key(&peer);
                self.connected.remove(&peer);
                self.seen_at.remove(&peer);
                self.progress.drop_peer(peer);
                if was_connected && let Err(err) = engine.note_peer_seen(peer) {
                    tracing::debug!(%peer, error = %err, "could not record when peer went offline");
                }
                events.push(SyncEvent::PeerDisconnected { peer });
                self.flush_progress(&mut events, true);
            }
            SyncInput::Frame { peer, body } => {
                self.on_frame(engine, peer, body, out, &mut events)?;
            }
            SyncInput::ObjectFetched { peer, object } => {
                self.on_object(engine, peer, object, Fetch::Ok, out, &mut events)?;
            }
            SyncInput::ObjectFetchFailed {
                peer,
                object,
                not_found,
                reason,
            } => {
                tracing::debug!(%peer, %object, not_found, %reason, "object fetch failed");
                self.on_object(
                    engine,
                    peer,
                    object,
                    Fetch::Failed { not_found },
                    out,
                    &mut events,
                )?;
            }
            SyncInput::ObjectProgress {
                peer,
                object,
                incoming,
                bytes,
            } => {
                if incoming {
                    self.progress
                        .note_download(peer, object, bytes, Instant::now());
                } else {
                    self.progress
                        .note_upload(peer, object, bytes, Instant::now());
                }
            }
            SyncInput::Fetch {
                space,
                mount,
                path,
                reply,
            } => {
                self.on_fetch_request(engine, &space, &mount, &path, reply, out, &mut events)?;
            }
            SyncInput::Rescan { .. }
            | SyncInput::AddPeer { .. }
            | SyncInput::PeerAddresses { .. }
            | SyncInput::NatHint { .. }
            | SyncInput::Config { .. } => {}
        }
        self.flush_progress(&mut events, false);
        Ok(events)
    }

    pub(super) fn flush_progress(&mut self, events: &mut Vec<SyncEvent>, force: bool) {
        if let Some(rows) = self.progress.emit_if_changed(Instant::now(), force) {
            events.push(SyncEvent::Transfers(rows));
        }
    }

    /// Rows currently open. Empty once a receive has applied through its plan
    /// and a send has been acknowledged through its plan.
    pub fn live_transfers(&self) -> Vec<TransferLive> {
        self.progress.snapshot()
    }

    /// Throttled snapshot for the watch loop, between sync inputs.
    pub fn poll_transfers(&mut self, now: Instant) -> Option<Vec<TransferLive>> {
        self.progress.emit_if_changed(now, false)
    }

    /// Peers with a live session.
    pub(crate) fn connected_peers(&self) -> Vec<DeviceId> {
        self.connected.keys().copied().collect()
    }

    /// Stop pushing `space` to `peer`. Batches already received stay queued;
    /// new ones are refused because the space is no longer shared.
    pub(crate) fn stop_sending(&mut self, peer: DeviceId, space: SpaceId) {
        if let Some(conn) = self.connected.get_mut(&peer) {
            conn.send_cursor.remove(&space);
        }
    }

    pub fn push_local_changes(
        &mut self,
        engine: &mut Engine,
        out: &mut dyn FnMut(SyncOutput),
    ) -> Result<Vec<SyncEvent>, EngineError> {
        let mut events = Vec::new();
        let peers: Vec<DeviceId> = self.connected.keys().copied().collect();
        for peer in peers {
            let spaces: Vec<SpaceId> = self
                .connected
                .get(&peer)
                .map(|c| c.send_cursor.keys().copied().collect())
                .unwrap_or_default();
            for space in spaces {
                self.send_batches(engine, peer, space, false, out, &mut events)?;
            }
        }
        self.flush_progress(&mut events, false);
        Ok(events)
    }

    pub fn tick(
        &mut self,
        engine: &mut Engine,
        now: Instant,
        out: &mut dyn FnMut(SyncOutput),
    ) -> Result<Vec<SyncEvent>, EngineError> {
        let mut events = Vec::new();
        self.touch_presence(engine, now);
        if self.hydration_due(now) {
            self.poll_hydration(engine, out, &mut events)?;
            self.hydrated_at = Some(now);
        }
        let peers: Vec<DeviceId> = self.connected.keys().copied().collect();
        for peer in peers {
            let spaces: Vec<SpaceId> = self
                .connected
                .get(&peer)
                .map(|c| c.incoming.keys().copied().collect())
                .unwrap_or_default();
            let resync_due: Vec<(SpaceId, u64)> = self
                .connected
                .get(&peer)
                .map(|c| {
                    c.resync_at
                        .iter()
                        .filter(|(_, at)| now >= **at)
                        .filter_map(|(space, _)| c.holes.get(space).map(|h| (*space, *h)))
                        .collect()
                })
                .unwrap_or_default();
            for (space, hole) in resync_due {
                if let Some(conn) = self.connected.get_mut(&peer) {
                    conn.resync_at.remove(&space);
                }
                out(index_request(peer, space, hole));
            }
            for space in spaces {
                let due = self
                    .connected
                    .get(&peer)
                    .and_then(|c| c.incoming.get(&space))
                    .and_then(|q| q.front())
                    .is_some_and(|b| b.retry_at.is_some_and(|t| now >= t));
                if due {
                    self.process_head(engine, peer, space, out, &mut events)?;
                }
            }
        }
        self.flush_progress(&mut events, false);
        Ok(events)
    }

    fn touch_presence(&mut self, engine: &mut Engine, now: Instant) {
        let due: Vec<DeviceId> = self
            .connected
            .keys()
            .copied()
            .filter(|peer| {
                self.seen_at
                    .get(peer)
                    .is_none_or(|at| now.saturating_duration_since(*at) >= SEEN_INTERVAL)
            })
            .collect();
        for peer in due {
            if let Err(err) = engine.note_peer_seen(peer) {
                tracing::debug!(%peer, error = %err, "could not refresh peer last seen");
                continue;
            }
            self.seen_at.insert(peer, now);
        }
    }

    fn on_connected(
        &mut self,
        engine: &mut Engine,
        peer: DeviceId,
        name: String,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        if engine.db.repo().peer_by_id(peer)?.is_none() {
            events.push(SyncEvent::SyncWarning {
                peer,
                path: String::new(),
                reason: format!("unknown peer {name}; ignoring connection"),
            });
            return Ok(());
        }
        engine.record_peer_name(peer, &name)?;
        self.connected.insert(
            peer,
            Connected {
                name: name.clone(),
                send_cursor: HashMap::new(),
                incoming: HashMap::new(),
                holes: HashMap::new(),
                resync_at: HashMap::new(),
                session_deletes: HashMap::new(),
                session_deleted_paths: HashMap::new(),
                held_emitted: HashSet::new(),
            },
        );
        events.push(SyncEvent::PeerConnected {
            peer,
            name: name.clone(),
        });
        self.seen_at.insert(peer, Instant::now());

        let offers = engine.space_offers_for_peer(peer)?;
        out(SyncOutput::Send {
            peer,
            body: frame::Body::SpaceOffers(offers),
        });

        for space_id in engine.db.repo().shared_space_ids(peer)? {
            if engine.db.repo().space(space_id)?.is_none() {
                continue;
            }
            resume_index(engine, peer, space_id, out)?;
        }
        Ok(())
    }

    fn on_frame(
        &mut self,
        engine: &mut Engine,
        peer: DeviceId,
        body: frame::Body,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        if !self.connected.contains_key(&peer) {
            return Ok(());
        }
        match body {
            frame::Body::SpaceOffers(offers) => {
                self.on_offers(engine, peer, offers, out, events)?
            }
            frame::Body::IndexRequest(req) => {
                self.on_index_request(engine, peer, req, out, events)?
            }
            frame::Body::IndexBatch(batch) => {
                self.on_index_batch(engine, peer, batch, out, events)?
            }
            frame::Body::Ack(ack) => {
                self.on_ack(engine, peer, ack)?;
                self.flush_progress(events, true);
            }
            frame::Body::Hello(_)
            | frame::Body::Ping(_)
            | frame::Body::Pong(_)
            | frame::Body::Error(_) => {}
        }
        Ok(())
    }
}
