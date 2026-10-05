//! Object transfer and hydration: batch objects, explicit fetches, and retry
//! across peers and the mailbox (D34, D35).

use super::*;

impl Syncer {
    pub(super) fn on_object(
        &mut self,
        engine: &mut Engine,
        peer: DeviceId,
        object: ObjectId,
        fetch: Fetch,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        match fetch {
            Fetch::Ok => {
                self.note_object_ready(engine, object, out, events)?;
                self.settle_direct(engine, object, events)
            }
            Fetch::Failed { not_found } => {
                self.on_fetch_failed(engine, peer, object, not_found, out, events)?;
                self.continue_direct(engine, peer, object, not_found, out, events)
            }
        }
    }

    /// A successful fetch fills every connected peer's batch that still lists
    /// `object`. Progress is credited to the batch's index source.
    fn note_object_ready(
        &mut self,
        engine: &mut Engine,
        object: ObjectId,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        let targets = self.spaces_pending_object(object);
        for (batch_peer, space) in targets {
            if !self.clear_pending_object(batch_peer, space, object) {
                continue;
            }
            self.progress
                .note_fetched(batch_peer, object, Instant::now());
            self.flush_progress(events, true);
            self.process_head(engine, batch_peer, space, out, events)?;
        }
        self.materialize_full_ready(engine, object, events)?;
        Ok(())
    }

    /// Consider index-only rows on the next tick, after a rule change.
    pub(crate) fn hydrate_soon(&mut self) {
        self.hydrated_at = None;
    }

    pub(super) fn hydration_due(&self, now: Instant) -> bool {
        self.hydrated_at
            .is_none_or(|at| now.saturating_duration_since(at) >= HYDRATE_INTERVAL)
    }

    pub(super) fn poll_hydration(
        &mut self,
        engine: &mut Engine,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        let pending = engine.full_unmaterialized()?;
        for item in pending {
            if item.object.is_none() {
                self.hydrate_now(engine, &item.key, events);
                continue;
            }
            let Some(object) = item.object else {
                continue;
            };
            if engine.store.contains(&object) {
                self.hydrate_now(engine, &item.key, events);
                continue;
            }
            if self
                .direct
                .get(&object)
                .is_some_and(|state| state.gave_up || state.waiting)
                || self.batch_pending(object)
            {
                continue;
            }
            if let Some(peer) = self.peer_for_space(engine, item.space)? {
                self.request_direct(peer, object, item.space, out);
            } else if engine.ingest_mailbox_object(item.space, object)? {
                self.hydrate_now(engine, &item.key, events);
            } else {
                self.give_up_direct(engine, object, item.key.path.as_ref(), events);
            }
        }
        Ok(())
    }

    fn hydrate_now(&mut self, engine: &mut Engine, key: &EntryKey, events: &mut Vec<SyncEvent>) {
        if let Err(err) = engine.materialize_indexed(key)
            && self.hydrate_warned.insert(key.clone())
        {
            events.push(SyncEvent::SyncWarning {
                peer: engine.device().id,
                path: key.path.to_string(),
                reason: err.to_string(),
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_fetch_request(
        &mut self,
        engine: &mut Engine,
        space: &str,
        mount: &str,
        path: &str,
        reply: mpsc::Sender<Result<(), Rejected>>,
        out: &mut dyn FnMut(SyncOutput),
        _events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        match engine.prepare_demand_fetch(space, mount, path) {
            Ok(FetchPrep::Done) => {
                let _ = reply.send(Ok(()));
            }
            Ok(FetchPrep::Need { space, object, key }) => {
                let Some(peer) = self.peer_for_space(engine, space)? else {
                    let _ = reply.send(Err((&EngineError::ObjectUnavailable).into()));
                    return Ok(());
                };
                self.wait_for_object(peer, space, object, Some(key), reply, out);
            }
            Err(err) => {
                let _ = reply.send(Err((&err).into()));
            }
        }
        Ok(())
    }

    /// Fill the store with `object` for a placeholder being opened (D43).
    pub(super) fn on_fetch_object(
        &mut self,
        engine: &mut Engine,
        space: SpaceId,
        object: ObjectId,
        reply: mpsc::Sender<Result<(), Rejected>>,
        out: &mut dyn FnMut(SyncOutput),
    ) -> Result<(), EngineError> {
        if engine.store.contains(&object) || engine.ingest_mailbox_object(space, object)? {
            let _ = reply.send(Ok(()));
            return Ok(());
        }
        let Some(peer) = self.peer_for_space(engine, space)? else {
            let _ = reply.send(Err((&EngineError::ObjectUnavailable).into()));
            return Ok(());
        };
        self.wait_for_object(peer, space, object, None, reply, out);
        Ok(())
    }

    fn wait_for_object(
        &mut self,
        peer: DeviceId,
        space: SpaceId,
        object: ObjectId,
        key: Option<EntryKey>,
        reply: mpsc::Sender<Result<(), Rejected>>,
        out: &mut dyn FnMut(SyncOutput),
    ) {
        self.fetch_waiters
            .entry(object)
            .or_default()
            .push(FetchWaiter { space, key, reply });
        if let Some(state) = self.direct.get_mut(&object) {
            state.gave_up = false;
            state.warned = false;
            state.space = Some(space);
        }
        let waiting = self.direct.get(&object).is_some_and(|state| state.waiting)
            || self.batch_pending(object);
        if !waiting {
            self.request_direct(peer, object, space, out);
        }
    }

    fn settle_direct(
        &mut self,
        engine: &mut Engine,
        object: ObjectId,
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        self.direct.remove(&object);
        self.materialize_full_ready(engine, object, events)?;
        if let Some(waiters) = self.fetch_waiters.remove(&object) {
            for waiter in waiters {
                let written = match &waiter.key {
                    Some(key) => engine.materialize_indexed(key),
                    None => Ok(()),
                };
                match written {
                    Ok(()) => {
                        let _ = waiter.reply.send(Ok(()));
                    }
                    Err(err) => {
                        let _ = waiter.reply.send(Err((&err).into()));
                    }
                }
            }
        }
        Ok(())
    }

    fn continue_direct(
        &mut self,
        engine: &mut Engine,
        failed: DeviceId,
        object: ObjectId,
        not_found: bool,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        if engine.store.contains(&object) {
            return self.settle_direct(engine, object, events);
        }
        if !self.direct.contains_key(&object) && !self.fetch_waiters.contains_key(&object) {
            return Ok(());
        }
        let space = self
            .fetch_waiters
            .get(&object)
            .and_then(|waiters| waiters.first().map(|waiter| waiter.space))
            .or_else(|| self.direct.get(&object).and_then(|state| state.space));
        let attempts = {
            let state = self.direct.entry(object).or_default();
            state.waiting = false;
            state.asked.insert(failed);
            state.attempts = state.attempts.saturating_add(1);
            if let Some(space) = space {
                state.space = Some(space);
            }
            state.attempts
        };
        let asked = self
            .direct
            .get(&object)
            .map(|state| state.asked.clone())
            .unwrap_or_default();
        if let Some(space) = space {
            if let Some(next) = self.unasked_connected(engine, space, &asked)? {
                self.request_direct(next, object, space, out);
                return Ok(());
            }
            if engine.ingest_mailbox_object(space, object)? {
                return self.settle_direct(engine, object, events);
            }
            if !not_found
                && attempts < MAX_FETCH_ATTEMPTS
                && let Some(peer) = self.peer_for_space(engine, space)?
            {
                self.request_direct(peer, object, space, out);
                return Ok(());
            }
        }
        let path = self
            .fetch_waiters
            .get(&object)
            .and_then(|waiters| waiters.first())
            .and_then(|waiter| waiter.key.as_ref())
            .map(|key| key.path.to_string())
            .unwrap_or_default();
        self.give_up_direct(engine, object, &path, events);
        Ok(())
    }

    fn materialize_full_ready(
        &mut self,
        engine: &mut Engine,
        object: ObjectId,
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        if !engine.store.contains(&object) {
            return Ok(());
        }
        if let Err(err) = engine.materialize_full_object(object) {
            events.push(SyncEvent::SyncWarning {
                peer: engine.device().id,
                path: String::new(),
                reason: err.to_string(),
            });
        }
        Ok(())
    }

    fn request_direct(
        &mut self,
        peer: DeviceId,
        object: ObjectId,
        space: SpaceId,
        out: &mut dyn FnMut(SyncOutput),
    ) {
        let state = self.direct.entry(object).or_default();
        state.space = Some(space);
        state.waiting = true;
        state.gave_up = false;
        state.asked.insert(peer);
        out(SyncOutput::FetchObject { peer, object });
    }

    fn give_up_direct(
        &mut self,
        engine: &Engine,
        object: ObjectId,
        path: &str,
        events: &mut Vec<SyncEvent>,
    ) {
        let state = self.direct.entry(object).or_default();
        state.waiting = false;
        state.gave_up = true;
        if !state.warned {
            state.warned = true;
            events.push(SyncEvent::SyncWarning {
                peer: engine.device().id,
                path: path.to_owned(),
                reason: format!("object {object} is not available to materialize"),
            });
        }
        if let Some(waiters) = self.fetch_waiters.remove(&object) {
            for waiter in waiters {
                let _ = waiter
                    .reply
                    .send(Err((&EngineError::ObjectUnavailable).into()));
            }
        }
    }

    fn batch_pending(&self, object: ObjectId) -> bool {
        self.connected.values().any(|conn| {
            conn.incoming.values().any(|queue| {
                queue
                    .iter()
                    .any(|batch| batch.pending_objects.contains(&object))
            })
        })
    }

    fn peer_for_space(
        &self,
        engine: &Engine,
        space: SpaceId,
    ) -> Result<Option<DeviceId>, EngineError> {
        let mut peers: Vec<DeviceId> = self.connected.keys().copied().collect();
        peers.sort();
        for peer in peers {
            if engine.db.repo().is_shared(space, peer)? {
                return Ok(Some(peer));
            }
        }
        Ok(None)
    }

    fn unasked_connected(
        &self,
        engine: &Engine,
        space: SpaceId,
        asked: &HashSet<DeviceId>,
    ) -> Result<Option<DeviceId>, EngineError> {
        let mut peers: Vec<DeviceId> = self
            .connected
            .keys()
            .copied()
            .filter(|peer| !asked.contains(peer))
            .collect();
        peers.sort();
        for peer in peers {
            if engine.db.repo().is_shared(space, peer)? {
                return Ok(Some(peer));
            }
        }
        Ok(None)
    }

    /// Other connected peers, then the mailbox, then the original peer's retry budget.
    fn on_fetch_failed(
        &mut self,
        engine: &mut Engine,
        peer: DeviceId,
        object: ObjectId,
        not_found: bool,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<(), EngineError> {
        let targets = self.failure_targets(object, peer);
        let mut emitted = HashSet::new();
        for (batch_peer, space, batch_id) in targets {
            if peer == batch_peer && not_found {
                self.mark_source_missing(batch_peer, space, batch_id, object);
            }
            if !self.note_failed_peer(batch_peer, space, batch_id, object, peer) {
                continue;
            }
            let asked = self.asked_peers(batch_peer, space, batch_id, object);
            if let Some(next) = self.unasked_peer(engine, space, batch_peer, &asked)? {
                if self.remember_ask(batch_peer, space, batch_id, object, next)
                    && emitted.insert(next)
                {
                    tracing::debug!(%batch_peer, %next, %object, "object fetch trying another peer");
                    out(SyncOutput::FetchObject { peer: next, object });
                }
                continue;
            }
            if let Some(bytes) = mailbox_object(engine, space, object)? {
                engine.store.put_bytes(&bytes)?;
                tracing::debug!(%object, %space, "object fetch used mailbox");
                self.note_object_ready(engine, object, out, events)?;
                return Ok(());
            }
            let source_missing = self.source_is_missing(batch_peer, space, batch_id, object);
            if self.retry_original(
                engine,
                QueuedBatch {
                    peer: batch_peer,
                    space,
                    id: batch_id,
                },
                object,
                source_missing,
                out,
                events,
            )? && emitted.insert(batch_peer)
            {
                out(SyncOutput::FetchObject {
                    peer: batch_peer,
                    object,
                });
            }
        }
        Ok(())
    }

    fn spaces_pending_object(&self, object: ObjectId) -> Vec<(DeviceId, SpaceId)> {
        let mut out = Vec::new();
        for (peer, conn) in &self.connected {
            for (space, queue) in &conn.incoming {
                if queue
                    .iter()
                    .any(|batch| batch.pending_objects.contains(&object))
                {
                    out.push((*peer, *space));
                }
            }
        }
        out
    }

    fn clear_pending_object(&mut self, peer: DeviceId, space: SpaceId, object: ObjectId) -> bool {
        let Some(queue) = self
            .connected
            .get_mut(&peer)
            .and_then(|conn| conn.incoming.get_mut(&space))
        else {
            return false;
        };
        let mut found = false;
        for batch in queue.iter_mut() {
            if batch.pending_objects.remove(&object) {
                found = true;
            }
        }
        found
    }

    fn failure_targets(&self, object: ObjectId, failed: DeviceId) -> Vec<(DeviceId, SpaceId, u64)> {
        let mut out = Vec::new();
        for (peer, conn) in &self.connected {
            for (space, queue) in &conn.incoming {
                for batch in queue {
                    if batch.pending_objects.contains(&object)
                        && batch
                            .asked
                            .get(&object)
                            .is_some_and(|asked| asked.contains(&failed))
                    {
                        out.push((*peer, *space, batch.id));
                    }
                }
            }
        }
        out
    }

    fn batch_by_id_mut(
        &mut self,
        peer: DeviceId,
        space: SpaceId,
        batch_id: u64,
    ) -> Option<&mut PendingBatch> {
        self.connected
            .get_mut(&peer)?
            .incoming
            .get_mut(&space)?
            .iter_mut()
            .find(|batch| batch.id == batch_id)
    }

    fn mark_source_missing(
        &mut self,
        batch_peer: DeviceId,
        space: SpaceId,
        batch_id: u64,
        object: ObjectId,
    ) {
        let Some(batch) = self.batch_by_id_mut(batch_peer, space, batch_id) else {
            return;
        };
        batch.source_missing.insert(object);
    }

    fn source_is_missing(
        &self,
        peer: DeviceId,
        space: SpaceId,
        batch_id: u64,
        object: ObjectId,
    ) -> bool {
        self.connected
            .get(&peer)
            .and_then(|conn| conn.incoming.get(&space))
            .and_then(|queue| queue.iter().find(|batch| batch.id == batch_id))
            .is_some_and(|batch| batch.source_missing.contains(&object))
    }

    fn note_failed_peer(
        &mut self,
        batch_peer: DeviceId,
        space: SpaceId,
        batch_id: u64,
        object: ObjectId,
        failed: DeviceId,
    ) -> bool {
        let Some(batch) = self.batch_by_id_mut(batch_peer, space, batch_id) else {
            return false;
        };
        if !batch.pending_objects.contains(&object) {
            return false;
        }
        let Some(asked) = batch.asked.get_mut(&object) else {
            return false;
        };
        if !asked.contains(&failed) {
            return false;
        }
        asked.insert(failed);
        true
    }

    fn remember_ask(
        &mut self,
        batch_peer: DeviceId,
        space: SpaceId,
        batch_id: u64,
        object: ObjectId,
        peer: DeviceId,
    ) -> bool {
        let Some(batch) = self.batch_by_id_mut(batch_peer, space, batch_id) else {
            return false;
        };
        if !batch.pending_objects.contains(&object) {
            return false;
        }
        batch.asked.entry(object).or_default().insert(peer);
        true
    }

    fn asked_peers(
        &self,
        peer: DeviceId,
        space: SpaceId,
        batch_id: u64,
        object: ObjectId,
    ) -> HashSet<DeviceId> {
        self.connected
            .get(&peer)
            .and_then(|conn| conn.incoming.get(&space))
            .and_then(|queue| queue.iter().find(|batch| batch.id == batch_id))
            .and_then(|batch| batch.asked.get(&object).cloned())
            .unwrap_or_default()
    }

    /// Smallest device id among connected peers that share `space` and have not
    /// been asked. The index source is never chosen here.
    fn unasked_peer(
        &self,
        engine: &Engine,
        space: SpaceId,
        batch_peer: DeviceId,
        asked: &HashSet<DeviceId>,
    ) -> Result<Option<DeviceId>, EngineError> {
        let mut candidates: Vec<DeviceId> = self
            .connected
            .keys()
            .copied()
            .filter(|id| *id != batch_peer && !asked.contains(id))
            .collect();
        candidates.sort();
        for id in candidates {
            if engine.db.repo().is_shared(space, id)? {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    /// `Ok(true)` means the caller should ask `batch.peer` again.
    fn retry_original(
        &mut self,
        engine: &mut Engine,
        batch: QueuedBatch,
        object: ObjectId,
        not_found: bool,
        out: &mut dyn FnMut(SyncOutput),
        events: &mut Vec<SyncEvent>,
    ) -> Result<bool, EngineError> {
        enum Step {
            Retry,
            Failed { retries: u64 },
        }
        let step = {
            let Some(pending) = self.batch_by_id_mut(batch.peer, batch.space, batch.id) else {
                return Ok(false);
            };
            if !pending.pending_objects.contains(&object) {
                return Ok(false);
            }
            let attempts = pending.fetch_attempts.entry(object).or_insert(0);
            *attempts += 1;
            if !not_found && *attempts < MAX_FETCH_ATTEMPTS {
                Step::Retry
            } else {
                pending.failed_objects.insert(object);
                pending.pending_objects.remove(&object);
                Step::Failed {
                    retries: pending.failed_objects.len() as u64,
                }
            }
        };
        match step {
            Step::Retry => Ok(true),
            Step::Failed { retries } => {
                self.progress.set_retries(batch.peer, batch.space, retries);
                self.process_head(engine, batch.peer, batch.space, out, events)?;
                Ok(false)
            }
        }
    }
}

fn mailbox_object(
    engine: &Engine,
    space: SpaceId,
    object: ObjectId,
) -> Result<Option<Vec<u8>>, EngineError> {
    let Some(path) = engine.replica_path()? else {
        return Ok(None);
    };
    let replica = open_replica(&path)?;
    match engine.take_space_object(&replica, space, object)? {
        MailboxRead::Ready(bytes) if ObjectId::of(&bytes) == object => Ok(Some(bytes)),
        MailboxRead::Ready(_) | MailboxRead::Locked | MailboxRead::Missing => Ok(None),
    }
}
