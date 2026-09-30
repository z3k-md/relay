//! Engine-native peer sync I/O. The CLI/daemon maps these 1:1 onto relay-net.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use relay_core::version::VectorOrdering;
use relay_core::{DeviceId, EntryContent, EntryKey, MountId, ObjectId, Sequence, SpaceId};
use relay_db::{OfferedMember, PeerOfferRow};
use relay_proto::{
    Ack, INDEX_BATCH_ENTRIES, IndexBatch, IndexRequest, RemoteEntry, device_id_from_bytes,
    entry_from_wire, entry_to_wire, frame, space_id_bytes, space_id_from_bytes,
};
use serde::Serialize;

use crate::Engine;
use crate::error::EngineError;
use crate::peers::offered_mounts_from_wire;
use crate::progress::{IncomingFile, ProgressBook, TransferLive};

const RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_ATTEMPTS: u32 = 3;
/// Fetches of one object that failed for a reason other than "not found"
/// before the entries needing it are left for a later re-request.
const MAX_FETCH_ATTEMPTS: u32 = 3;
/// Delay before re-requesting a range whose objects could not be fetched.
const RESYNC_DELAY: Duration = Duration::from_secs(30);

/// Result of a live [`SyncInput::AddMount`] applied on the engine loop.
#[derive(Clone, Debug)]
pub struct AddMountApplied {
    pub name: String,
    pub path: Option<PathBuf>,
    pub space_id: SpaceId,
    pub mount_id: MountId,
}

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
    /// Add a local mount through the loop writer (D19 / D24 / D25).
    AddMount {
        space: String,
        mount: String,
        path: PathBuf,
        reply: mpsc::Sender<Result<AddMountApplied, String>>,
    },
    /// Share a space with a peer through the loop writer.
    Share {
        space: String,
        peer: String,
        reply: mpsc::Sender<Result<(), String>>,
    },
}

#[derive(Clone, Debug)]
pub enum SyncOutput {
    Send { peer: DeviceId, body: frame::Body },
    FetchObject { peer: DeviceId, object: ObjectId },
    SetPeers,
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

struct PendingBatch {
    after_sequence: u64,
    through_sequence: u64,
    entries: Vec<RemoteEntry>,
    pending_objects: HashSet<ObjectId>,
    failed_objects: HashSet<ObjectId>,
    fetch_attempts: HashMap<ObjectId, u32>,
    attempts: u32,
    retry_at: Option<Instant>,
    caught_up: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fetch {
    Ok,
    Failed { not_found: bool },
}

pub struct Syncer {
    connected: HashMap<DeviceId, Connected>,
    index_batch_entries: usize,
    progress: ProgressBook,
}

impl Default for Syncer {
    fn default() -> Self {
        Self {
            connected: HashMap::new(),
            index_batch_entries: INDEX_BATCH_ENTRIES,
            progress: ProgressBook::default(),
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
                self.connected.remove(&peer);
                self.progress.drop_peer(peer);
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
            SyncInput::Rescan { .. }
            | SyncInput::AddPeer { .. }
            | SyncInput::PeerAddresses { .. }
            | SyncInput::AddMount { .. }
            | SyncInput::Share { .. } => {}
        }
        self.flush_progress(&mut events, false);
        Ok(events)
    }

    fn flush_progress(&mut self, events: &mut Vec<SyncEvent>, force: bool) {
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

    /// Re-send current space offers to a connected peer after a live share or
    /// mount change. No-op if that peer is not connected.
    pub fn refresh_offers(
        &self,
        engine: &Engine,
        peer: DeviceId,
        out: &mut dyn FnMut(SyncOutput),
    ) -> Result<(), EngineError> {
        if !self.connected.contains_key(&peer) {
            return Ok(());
        }
        let offers = engine.space_offers_for_peer(peer)?;
        out(SyncOutput::Send {
            peer,
            body: frame::Body::SpaceOffers(offers),
        });
        Ok(())
    }

    /// Refresh offers for every connected peer that currently shares `space`.
    pub fn refresh_offers_for_space(
        &self,
        engine: &Engine,
        space: SpaceId,
        out: &mut dyn FnMut(SyncOutput),
    ) -> Result<(), EngineError> {
        let peers: Vec<DeviceId> = self.connected.keys().copied().collect();
        for peer in peers {
            if engine.db.repo().is_shared(space, peer)? {
                self.refresh_offers(engine, peer, out)?;
            }
        }
        Ok(())
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
                out(SyncOutput::Send {
                    peer,
                    body: frame::Body::IndexRequest(IndexRequest {
                        space_id: space_id_bytes(&space),
                        after_sequence: hole,
                    }),
                });
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

        let offers = engine.space_offers_for_peer(peer)?;
        out(SyncOutput::Send {
            peer,
            body: frame::Body::SpaceOffers(offers),
        });

        for space_id in engine.db.repo().shared_space_ids(peer)? {
            if engine.db.repo().space(space_id)?.is_none() {
                continue;
            }
            let after = engine.db.repo().sync_progress(peer, space_id)?.received_seq;
            out(SyncOutput::Send {
                peer,
                body: frame::Body::IndexRequest(IndexRequest {
                    space_id: space_id_bytes(&space_id),
                    after_sequence: after.0,
                }),
            });
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

    fn on_offers(
        &mut self,
        engine: &mut Engine,
        peer: DeviceId,
        offers: relay_proto::SpaceOffers,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        let mut rows = Vec::new();
        let mut listed = Vec::new();
        let mut set_peers = false;
        let mut follow_up: Vec<(DeviceId, SpaceId)> = Vec::new();
        let mut policy_replays: Vec<SpaceId> = Vec::new();
        for offer in offers.spaces {
            let Ok(space_id) = space_id_from_bytes(&offer.space_id) else {
                events.push(SyncEvent::SyncWarning {
                    peer,
                    path: offer.name.clone(),
                    reason: "invalid space id in offer".into(),
                });
                continue;
            };
            let mounts = match offered_mounts_from_wire(&offer.mounts) {
                Ok(m) => m,
                Err(err) => {
                    events.push(SyncEvent::SyncWarning {
                        peer,
                        path: offer.name.clone(),
                        reason: err.to_string(),
                    });
                    continue;
                }
            };
            let mut members = Vec::new();
            for member in &offer.members {
                match device_id_from_bytes(&member.device_id) {
                    Ok(id) => members.push(OfferedMember {
                        id,
                        name: member.name.clone(),
                        addresses: member.addresses.clone(),
                    }),
                    Err(_) => {
                        events.push(SyncEvent::SyncWarning {
                            peer,
                            path: offer.name.clone(),
                            reason: "invalid member device id in offer".into(),
                        });
                    }
                }
            }
            let already = engine.db.repo().space(space_id)?.is_some();
            listed.push(OfferedSpaceEvent {
                name: offer.name.clone(),
                id: space_id,
                already_joined: already,
            });
            if already {
                let adopted = engine.adopt_offered_members(space_id, &members)?;
                if adopted.peers_changed {
                    set_peers = true;
                }
                for id in adopted.newly_shared {
                    if self.connected.contains_key(&id) {
                        follow_up.push((id, space_id));
                    }
                }
                if let Some(replay) =
                    self.apply_policy_offer(engine, peer, space_id, &offer, out, events)?
                {
                    policy_replays.push(replay);
                }
            }
            rows.push(PeerOfferRow {
                space_id,
                name: offer.name,
                mounts,
                members,
            });
        }
        engine.persist_offers(peer, &rows)?;
        if set_peers {
            out(SyncOutput::SetPeers);
        }
        for (member, space) in follow_up {
            self.refresh_offers(engine, member, out)?;
            let needs_index = self
                .connected
                .get(&member)
                .is_some_and(|c| !c.send_cursor.contains_key(&space));
            if needs_index {
                let after = engine.db.repo().sync_progress(member, space)?.received_seq;
                out(SyncOutput::Send {
                    peer: member,
                    body: frame::Body::IndexRequest(IndexRequest {
                        space_id: space_id_bytes(&space),
                        after_sequence: after.0,
                    }),
                });
            }
        }
        for space in policy_replays {
            if let Some(conn) = self.connected.get_mut(&peer) {
                conn.send_cursor.insert(space, Sequence::ZERO);
            }
            self.send_batches(engine, peer, space, true, out, events)?;
            out(SyncOutput::Send {
                peer,
                body: frame::Body::IndexRequest(IndexRequest {
                    space_id: space_id_bytes(&space),
                    after_sequence: 0,
                }),
            });
        }
        let hint: Vec<_> = listed
            .iter()
            .filter(|s| !s.already_joined)
            .cloned()
            .collect();
        if !hint.is_empty() {
            events.push(SyncEvent::OffersReceived { peer, spaces: hint });
        }
        Ok(())
    }

    /// Store a peer's policy snapshot when the epoch changes. Returns the space
    /// id when both sides should replay from sequence 0.
    fn apply_policy_offer(
        &mut self,
        engine: &mut Engine,
        peer: DeviceId,
        space: SpaceId,
        offer: &relay_proto::SpaceOffer,
        _out: &mut dyn FnMut(SyncOutput),
        _events: &mut Vec<SyncEvent>,
    ) -> Result<Option<SpaceId>, EngineError> {
        let previous = engine.db.repo().peer_policy_snapshot(peer, space)?;
        if previous
            .as_ref()
            .is_some_and(|s| s.epoch == offer.policy_epoch)
        {
            return Ok(None);
        }
        let first_empty =
            previous.is_none() && offer.policy_epoch == 0 && offer.policies.is_empty();
        engine.store_peer_policy_snapshot(peer, space, offer.policy_epoch, &offer.policies)?;
        if first_empty {
            return Ok(None);
        }
        Ok(Some(space))
    }

    fn on_index_request(
        &mut self,
        engine: &mut Engine,
        peer: DeviceId,
        req: IndexRequest,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        let space = match space_id_from_bytes(&req.space_id) {
            Ok(id) => id,
            Err(_) => return Ok(()),
        };
        if engine.db.repo().space(space)?.is_none() || !engine.db.repo().is_shared(space, peer)? {
            return Ok(());
        }
        if let Some(conn) = self.connected.get_mut(&peer) {
            conn.send_cursor.insert(space, Sequence(req.after_sequence));
        }
        self.send_batches(engine, peer, space, true, out, events)
    }

    fn send_batches(
        &mut self,
        engine: &mut Engine,
        peer: DeviceId,
        space: SpaceId,
        force_empty: bool,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        let latest = engine.db.repo().latest_sequence()?;
        let mut after = self
            .connected
            .get(&peer)
            .and_then(|c| c.send_cursor.get(&space).copied())
            .unwrap_or(Sequence::ZERO);
        let space_name = engine
            .db
            .repo()
            .space(space)?
            .map(|s| s.name)
            .unwrap_or_else(|| space.to_string());

        let plan_after = after.0;
        let plan = if after.0 < latest.0 {
            engine.db.repo().catchup_plan(space, after)?
        } else {
            relay_db::CatchupPlan { files: 0, bytes: 0 }
        };
        if plan.files > 0 {
            let peer_name = self
                .connected
                .get(&peer)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let now_ms = u64::try_from(engine.clock.now_ms()).unwrap_or(0);
            self.progress.begin_send(
                peer,
                &peer_name,
                space,
                &space_name,
                plan.files,
                plan.bytes,
                plan_after,
                latest.0,
                now_ms,
            );
            self.flush_progress(events, true);
        }
        let stamped = (plan.files > 0).then_some((plan.files, plan.bytes, plan_after));

        let limit = self.index_batch_entries;
        loop {
            let changes = engine
                .db
                .repo()
                .changes_since_in_space(space, after, limit)?;
            let query_len = changes.len();
            if changes.is_empty() && !force_empty && after.0 >= latest.0 {
                break;
            }
            let through = if changes.len() < limit {
                latest
            } else {
                changes.last().map(|e| e.sequence).unwrap_or(after)
            };
            let caught_up = through.0 >= latest.0;

            let mounts = engine.db.repo().list_mounts(Some(space))?;
            let mount_names: HashMap<MountId, String> = mounts
                .into_iter()
                .map(|cfg| (cfg.mount.id, cfg.mount.name))
                .collect();
            let mut wire_entries = Vec::new();
            let mut objects = Vec::new();
            for entry in &changes {
                let include = match mount_names.get(&entry.key.mount) {
                    Some(mount_name) => {
                        engine.wants(space, peer, mount_name, entry.key.path.as_str())?
                    }
                    None => true,
                };
                if !include {
                    continue;
                }
                if let EntryContent::File { object, size, .. } = &entry.content {
                    objects.push((*object, *size));
                }
                wire_entries.push(entry_to_wire(entry));
            }
            let n = wire_entries.len();
            let batch = IndexBatch {
                space_id: space_id_bytes(&space),
                entries: wire_entries,
                through_sequence: through.0,
                caught_up,
                after_sequence: after.0,
                plan_files: stamped.map(|(f, _, _)| f),
                plan_bytes: stamped.map(|(_, b, _)| b),
                plan_after: stamped.map(|(_, _, a)| a),
            };
            out(SyncOutput::Send {
                peer,
                body: frame::Body::IndexBatch(batch),
            });
            if n > 0 {
                self.progress
                    .note_sent(peer, space, through.0, n as u64, &objects);
                events.push(SyncEvent::SentChanges {
                    peer,
                    space: space_name.clone(),
                    entries: n,
                });
            }
            if let Some(conn) = self.connected.get_mut(&peer) {
                conn.send_cursor.insert(space, through);
            }
            after = through;
            if caught_up {
                break;
            }
            // Only force one empty batch on the initial IndexRequest.
            // Use the pre-filter query length so a fully-filtered batch does
            // not stall the cursor.
            if force_empty && query_len == 0 {
                break;
            }
        }
        Ok(())
    }

    fn on_index_batch(
        &mut self,
        engine: &mut Engine,
        peer: DeviceId,
        batch: IndexBatch,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        let space = match space_id_from_bytes(&batch.space_id) {
            Ok(id) => id,
            Err(_) => return Ok(()),
        };
        if engine.db.repo().space(space)?.is_none() || !engine.db.repo().is_shared(space, peer)? {
            return Ok(());
        }

        let mut entries = Vec::new();
        for wire in batch.entries {
            match entry_from_wire(space, wire) {
                Ok(entry) => entries.push(entry),
                Err(err) => events.push(SyncEvent::SyncWarning {
                    peer,
                    path: String::new(),
                    reason: format!("skipping bad entry: {err}"),
                }),
            }
        }

        let mounts = engine.db.repo().list_mounts(Some(space))?;
        let mount_names: HashMap<MountId, String> = mounts
            .into_iter()
            .map(|cfg| (cfg.mount.id, cfg.mount.name))
            .collect();
        let local = engine.device().id;
        let mut kept = Vec::with_capacity(entries.len());
        for entry in entries {
            match mount_names.get(&entry.key.mount) {
                Some(name) => {
                    if engine.wants(space, local, name, entry.key.path.as_str())? {
                        kept.push(entry);
                    }
                }
                None => kept.push(entry),
            }
        }
        let entries = kept;

        let mut pending = HashSet::new();
        for entry in &entries {
            if let Some(obj) = entry.content.object()
                && !engine.store.contains(&obj)
            {
                pending.insert(obj);
            }
        }
        for obj in &pending {
            out(SyncOutput::FetchObject { peer, object: *obj });
        }

        let incoming_files: Vec<IncomingFile> = entries
            .iter()
            .filter_map(|entry| match &entry.content {
                EntryContent::File { object, size, .. } => Some(IncomingFile {
                    sequence: entry.sequence.0,
                    path: entry.key.path.as_str().to_owned(),
                    object: *object,
                    size: *size,
                    local: !pending.contains(object),
                }),
                _ => None,
            })
            .collect();
        let has_entries = !entries.is_empty();
        let peer_name = self
            .connected
            .get(&peer)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let space_name = engine
            .db
            .repo()
            .space(space)?
            .map(|s| s.name)
            .unwrap_or_else(|| space.to_string());
        let now_ms = u64::try_from(engine.clock.now_ms()).unwrap_or(0);
        self.progress.observe_incoming(
            peer,
            &peer_name,
            space,
            &space_name,
            batch.plan_files,
            batch.plan_bytes,
            batch.plan_after,
            &incoming_files,
            has_entries,
            now_ms,
        );
        self.flush_progress(events, true);

        let pending_batch = PendingBatch {
            after_sequence: batch.after_sequence,
            through_sequence: batch.through_sequence,
            entries,
            pending_objects: pending,
            failed_objects: HashSet::new(),
            fetch_attempts: HashMap::new(),
            attempts: 0,
            retry_at: None,
            caught_up: batch.caught_up,
        };
        if let Some(conn) = self.connected.get_mut(&peer) {
            conn.incoming
                .entry(space)
                .or_default()
                .push_back(pending_batch);
        }
        self.process_head(engine, peer, space, out, events)
    }

    fn on_object(
        &mut self,
        engine: &mut Engine,
        peer: DeviceId,
        object: ObjectId,
        fetch: Fetch,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        let spaces: Vec<SpaceId> = self
            .connected
            .get(&peer)
            .map(|c| c.incoming.keys().copied().collect())
            .unwrap_or_default();
        for space in spaces {
            if let Some(conn) = self.connected.get_mut(&peer)
                && let Some(queue) = conn.incoming.get_mut(&space)
                && let Some(head) = queue.front_mut()
                && head.pending_objects.contains(&object)
            {
                if let Fetch::Failed { not_found } = fetch {
                    let attempts = head.fetch_attempts.entry(object).or_insert(0);
                    *attempts += 1;
                    if !not_found && *attempts < MAX_FETCH_ATTEMPTS {
                        out(SyncOutput::FetchObject { peer, object });
                        continue;
                    }
                    head.failed_objects.insert(object);
                    let retries = head.failed_objects.len() as u64;
                    let _ = head;
                    self.progress.set_retries(peer, space, retries);
                } else {
                    let _ = head;
                    self.progress.note_fetched(peer, object, Instant::now());
                    self.flush_progress(events, true);
                }
                if let Some(conn) = self.connected.get_mut(&peer)
                    && let Some(queue) = conn.incoming.get_mut(&space)
                    && let Some(head) = queue.front_mut()
                {
                    head.pending_objects.remove(&object);
                }
                self.process_head(engine, peer, space, out, events)?;
            }
        }
        Ok(())
    }

    fn on_ack(&mut self, engine: &mut Engine, peer: DeviceId, ack: Ack) -> Result<(), EngineError> {
        let space = match space_id_from_bytes(&ack.space_id) {
            Ok(id) => id,
            Err(_) => return Ok(()),
        };
        let now = engine.clock.now_ms();
        engine
            .db
            .transaction(|repo| {
                repo.set_acked_seq(peer, space, Sequence(ack.through_sequence), now)
            })
            .map_err(EngineError::from_db)?;
        self.progress.note_ack(peer, space, ack.through_sequence);
        Ok(())
    }

    fn process_head(
        &mut self,
        engine: &mut Engine,
        peer: DeviceId,
        space: SpaceId,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        let Some(conn) = self.connected.get_mut(&peer) else {
            return Ok(());
        };
        let Some(queue) = conn.incoming.get_mut(&space) else {
            return Ok(());
        };
        let Some(head) = queue.front_mut() else {
            return Ok(());
        };
        if !head.pending_objects.is_empty() {
            return Ok(());
        }
        if head.retry_at.is_some() && head.retry_at.is_some_and(|t| Instant::now() < t) {
            return Ok(());
        }

        let entries = head.entries.clone();
        let failed = head.failed_objects.clone();
        let applied: Vec<u64> = entries
            .iter()
            .filter(|entry| {
                entry
                    .content
                    .object()
                    .is_none_or(|object| !failed.contains(&object))
            })
            .map(|entry| entry.sequence.0)
            .collect();
        let batch_after = head.after_sequence;
        let batch_through = head.through_sequence;
        let caught_up = head.caught_up;
        let failed_min = entries
            .iter()
            .filter(|e| e.content.object().is_some_and(|o| failed.contains(&o)))
            .map(|e| e.sequence.0)
            .min();

        let batch_deletes = live_tombstones(engine, &entries)?;
        match self.mass_delete_guard(engine, peer, space, &batch_deletes, events)? {
            MassDeleteAction::Hold => return Ok(()),
            MassDeleteAction::Proceed => {}
        }

        let outcome = engine.apply_remote_batch(peer, space, entries.clone(), &failed)?;
        for w in &outcome.warnings {
            events.push(SyncEvent::SyncWarning {
                peer,
                path: w.path.clone(),
                reason: w.reason.clone(),
            });
        }

        if outcome.transient {
            if let Some(conn) = self.connected.get_mut(&peer)
                && let Some(queue) = conn.incoming.get_mut(&space)
                && let Some(head) = queue.front_mut()
            {
                head.attempts += 1;
                if head.attempts >= MAX_ATTEMPTS {
                    let skipped = head.entries.len();
                    events.push(SyncEvent::SyncWarning {
                        peer,
                        path: String::new(),
                        reason: format!("giving up on batch after {MAX_ATTEMPTS} attempts ({skipped} entries skipped)"),
                    });
                    queue.pop_front();
                    return self.process_head(engine, peer, space, out, events);
                }
                head.retry_at = Some(Instant::now() + RETRY_DELAY);
            }
            return Ok(());
        }

        if let Some(conn) = self.connected.get_mut(&peer) {
            for (mount, paths) in &batch_deletes {
                *conn.session_deletes.entry((space, *mount)).or_default() += paths.len();
            }
            for (mount, paths) in applied_tombstone_paths(engine, space, &batch_deletes)? {
                conn.session_deleted_paths
                    .entry((space, mount))
                    .or_default()
                    .extend(paths);
            }
            if caught_up {
                conn.session_deletes.retain(|(s, _), _| *s != space);
                conn.session_deleted_paths.retain(|(s, _), _| *s != space);
            }
        }
        if caught_up {
            engine.clear_delete_holds_for_peer_space(peer, space)?;
        }

        let through = match self.connected.get_mut(&peer) {
            Some(conn) => {
                if let Some(min) = failed_min {
                    let hole = min.saturating_sub(1);
                    let hole = conn.holes.get(&space).map_or(hole, |h| (*h).min(hole));
                    conn.holes.insert(space, hole);
                    conn.resync_at
                        .entry(space)
                        .or_insert_with(|| Instant::now() + RESYNC_DELAY);
                    events.push(SyncEvent::SyncWarning {
                        peer,
                        path: String::new(),
                        reason: format!(
                            "{} objects could not be fetched; will re-request them",
                            failed.len()
                        ),
                    });
                } else if conn.holes.get(&space).is_some_and(|h| batch_after <= *h) {
                    conn.holes.remove(&space);
                    conn.resync_at.remove(&space);
                }
                conn.holes
                    .get(&space)
                    .map_or(batch_through, |h| (*h).min(batch_through))
            }
            None => batch_through,
        };
        let now = engine.clock.now_ms();
        engine
            .db
            .transaction(|repo| repo.set_received_seq(peer, space, Sequence(through), now))?;

        out(SyncOutput::Send {
            peer,
            body: frame::Body::Ack(Ack {
                space_id: space_id_bytes(&space),
                through_sequence: through,
            }),
        });

        let space_name = engine
            .db
            .repo()
            .space(space)?
            .map(|s| s.name)
            .unwrap_or_else(|| space.to_string());
        let mount = engine
            .db
            .repo()
            .list_mounts(Some(space))?
            .into_iter()
            .next()
            .map(|c| c.mount.name)
            .unwrap_or_default();
        events.push(SyncEvent::RemoteApplied {
            peer,
            space: space_name,
            mount,
            written: outcome.written,
            deleted: outcome.deleted,
            conflicts: outcome.conflicts,
            skipped: outcome.skipped,
        });

        if let Some(conn) = self.connected.get_mut(&peer)
            && let Some(queue) = conn.incoming.get_mut(&space)
        {
            queue.pop_front();
        }
        let queue_empty = self
            .connected
            .get(&peer)
            .and_then(|conn| conn.incoming.get(&space))
            .is_none_or(|queue| queue.is_empty());
        self.progress
            .note_applied(peer, space, &applied, caught_up, queue_empty);
        self.flush_progress(events, true);
        self.process_head(engine, peer, space, out, events)
    }

    fn mass_delete_guard(
        &mut self,
        engine: &mut Engine,
        peer: DeviceId,
        space: SpaceId,
        batch_deletes: &HashMap<MountId, Vec<relay_core::LogicalPath>>,
        events: &mut Vec<SyncEvent>,
    ) -> Result<MassDeleteAction, EngineError> {
        let holds: Vec<_> = engine
            .db
            .repo()
            .list_delete_holds()?
            .into_iter()
            .filter(|h| h.peer.id == peer && h.space.id == space)
            .collect();

        let mut mounts: HashSet<MountId> = batch_deletes.keys().copied().collect();
        for hold in &holds {
            mounts.insert(hold.mount.id);
        }

        // A decided hold stays until a `caught_up` batch clears it, so the
        // decision covers every batch of the catch-up, across reconnects.
        let mut restore_mounts = Vec::new();
        let mut new_holds = Vec::new();
        let mut held = false;
        for mount in mounts {
            let hold = holds.iter().find(|h| h.mount.id == mount);
            match hold.and_then(|h| h.decision) {
                Some(relay_db::DeleteHoldDecision::Apply) => {}
                Some(relay_db::DeleteHoldDecision::Restore) => restore_mounts.push(mount),
                None if hold.is_some() => held = true,
                None => {
                    let batch_n = batch_deletes.get(&mount).map(Vec::len).unwrap_or(0);
                    let session = self
                        .connected
                        .get(&peer)
                        .and_then(|c| c.session_deletes.get(&(space, mount)).copied())
                        .unwrap_or(0);
                    let live = engine.db.repo().count_live(mount)?;
                    let baseline = live.saturating_add(session);
                    if crate::reports::is_large_fraction_delete(
                        session.saturating_add(batch_n),
                        baseline,
                    ) {
                        new_holds.push((mount, session.saturating_add(batch_n), baseline));
                        held = true;
                    }
                }
            }
        }

        if !new_holds.is_empty() {
            let queued_deletes = match self
                .connected
                .get(&peer)
                .and_then(|c| c.incoming.get(&space))
            {
                Some(queue) => {
                    let mut out: HashMap<MountId, Vec<relay_core::LogicalPath>> = HashMap::new();
                    for batch in queue {
                        for (mount, paths) in live_tombstones(engine, &batch.entries)? {
                            out.entry(mount).or_default().extend(paths);
                        }
                    }
                    out
                }
                None => HashMap::new(),
            };
            for (mount, deletions, live) in new_holds {
                let mut paths = queued_deletes.get(&mount).cloned().unwrap_or_default();
                paths.sort();
                paths.dedup();
                let mut applied = self
                    .connected
                    .get(&peer)
                    .and_then(|c| c.session_deleted_paths.get(&(space, mount)))
                    .cloned()
                    .unwrap_or_default();
                applied.sort();
                applied.dedup();
                engine.persist_delete_hold(
                    peer,
                    space,
                    mount,
                    deletions,
                    live,
                    (&paths, &applied),
                )?;
                let already = self
                    .connected
                    .get(&peer)
                    .is_some_and(|c| c.held_emitted.contains(&(space, mount)));
                if !already {
                    if let Some(conn) = self.connected.get_mut(&peer) {
                        conn.held_emitted.insert((space, mount));
                    }
                    events.push(delete_held_event(
                        engine, peer, space, mount, deletions, live,
                    )?);
                }
            }
        }

        if held {
            return Ok(MassDeleteAction::Hold);
        }

        for mount in restore_mounts {
            let mut paths = engine
                .db
                .repo()
                .list_delete_hold_paths(peer, space, mount)?;
            let applied = engine
                .db
                .repo()
                .list_delete_hold_applied_paths(peer, space, mount)?;
            let had_held_paths = !paths.is_empty() || !applied.is_empty();
            if let Some(batch) = batch_deletes.get(&mount) {
                paths.extend(batch.iter().cloned());
            }
            paths.sort();
            paths.dedup();
            let keys: Vec<EntryKey> = paths
                .into_iter()
                .map(|path| EntryKey { space, mount, path })
                .collect();
            if !keys.is_empty() {
                engine.reassert_live(&keys)?;
            }
            if had_held_paths && !applied.is_empty() {
                let applied_keys: Vec<EntryKey> = applied
                    .into_iter()
                    .map(|path| EntryKey { space, mount, path })
                    .collect();
                for (path, reason) in engine.resurrect_applied_deletes(peer, &applied_keys)? {
                    events.push(SyncEvent::SyncWarning { peer, path, reason });
                }
            }
            if had_held_paths {
                engine.clear_delete_hold_paths(peer, space, mount)?;
            }
        }
        Ok(MassDeleteAction::Proceed)
    }
}

enum MassDeleteAction {
    Proceed,
    Hold,
}

fn live_tombstones(
    engine: &Engine,
    entries: &[relay_proto::RemoteEntry],
) -> Result<HashMap<MountId, Vec<relay_core::LogicalPath>>, EngineError> {
    let mut out: HashMap<MountId, Vec<relay_core::LogicalPath>> = HashMap::new();
    for entry in entries {
        if !matches!(entry.content, EntryContent::Deleted) {
            continue;
        }
        let Some(local) = engine.db.repo().entry(&entry.key)? else {
            continue;
        };
        // Concurrent tombstones lose to the live side (D18) and stale ones are
        // ignored; only a dominating tombstone deletes the local file.
        if local.is_deleted() || entry.vector.compare(&local.vector) != VectorOrdering::Dominates {
            continue;
        }
        out.entry(entry.key.mount)
            .or_default()
            .push(entry.key.path.clone());
    }
    Ok(out)
}

fn applied_tombstone_paths(
    engine: &Engine,
    space: SpaceId,
    batch_deletes: &HashMap<MountId, Vec<relay_core::LogicalPath>>,
) -> Result<HashMap<MountId, Vec<relay_core::LogicalPath>>, EngineError> {
    let mut out: HashMap<MountId, Vec<relay_core::LogicalPath>> = HashMap::new();
    for (mount, paths) in batch_deletes {
        for path in paths {
            let key = EntryKey {
                space,
                mount: *mount,
                path: path.clone(),
            };
            let Some(local) = engine.db.repo().entry(&key)? else {
                continue;
            };
            if local.is_deleted() {
                out.entry(*mount).or_default().push(path.clone());
            }
        }
    }
    Ok(out)
}

fn delete_held_event(
    engine: &Engine,
    peer: DeviceId,
    space: SpaceId,
    mount: MountId,
    deletions: usize,
    live: usize,
) -> Result<SyncEvent, EngineError> {
    let space_name = engine
        .db
        .repo()
        .space(space)?
        .map(|s| s.name)
        .unwrap_or_else(|| space.to_string());
    let mount_name = engine
        .db
        .repo()
        .mount_config(mount)?
        .map(|c| c.mount.name)
        .unwrap_or_else(|| mount.to_string());
    Ok(SyncEvent::DeletesHeld {
        peer,
        space: space_name,
        mount: mount_name,
        deletions,
        live,
    })
}
