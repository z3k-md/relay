//! Index exchange: requests, batches out, batches in, acks, and applying the
//! head of each peer's queue (D16, D17).

use super::*;

impl Syncer {
    /// Ask every connected member of `space` for its index from the stored
    /// watermark. After a live join or attach: entries for a mount with no
    /// local path were skipped (D16), and attaching reset the watermark.
    pub(crate) fn request_index(
        &self,
        engine: &Engine,
        space: SpaceId,
        out: &mut dyn FnMut(SyncOutput),
    ) -> Result<(), EngineError> {
        for &peer in self.connected.keys() {
            if engine.db.repo().is_shared(space, peer)? {
                resume_index(engine, peer, space, out)?;
            }
        }
        Ok(())
    }

    pub(super) fn on_index_request(
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

    pub(super) fn send_batches(
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

    pub(super) fn on_index_batch(
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
        let rules = engine.rules_for(space)?;
        let mut kept = Vec::with_capacity(entries.len());
        for entry in entries {
            match mount_names.get(&entry.key.mount) {
                Some(name) => {
                    if !engine.wants(space, local, name, entry.key.path.as_str())? {
                        continue;
                    }
                    let mode = path_mode(&rules, name, entry.key.path.as_str())?;
                    if mode == MaterializationMode::Exclude {
                        continue;
                    }
                    kept.push(entry);
                }
                None => kept.push(entry),
            }
        }
        let entries = kept;

        let mut pending = HashSet::new();
        for entry in &entries {
            let Some(obj) = entry.content.object() else {
                continue;
            };
            if engine.store.contains(&obj) {
                continue;
            }
            let mode = match mount_names.get(&entry.key.mount) {
                Some(name) => path_mode(&rules, name, entry.key.path.as_str())?,
                None => MaterializationMode::Full,
            };
            if engine.needs_object_bytes(mode, &entry.key)? {
                pending.insert(obj);
            }
        }
        let mut asked = HashMap::new();
        for obj in &pending {
            asked.insert(*obj, HashSet::from([peer]));
            out(SyncOutput::FetchObject { peer, object: *obj });
        }

        let incoming_files: Vec<IncomingFile> = entries
            .iter()
            .filter_map(|entry| match &entry.content {
                EntryContent::File { object, size, .. } if pending.contains(object) => {
                    Some(IncomingFile {
                        sequence: entry.sequence.0,
                        path: entry.key.path.as_str().to_owned(),
                        object: *object,
                        size: *size,
                        local: false,
                    })
                }
                EntryContent::File { object, size, .. } if engine.store.contains(object) => {
                    Some(IncomingFile {
                        sequence: entry.sequence.0,
                        path: entry.key.path.as_str().to_owned(),
                        object: *object,
                        size: *size,
                        local: true,
                    })
                }
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

        let batch_id = self.next_batch_id;
        self.next_batch_id = self.next_batch_id.wrapping_add(1);
        let pending_batch = PendingBatch {
            id: batch_id,
            after_sequence: batch.after_sequence,
            through_sequence: batch.through_sequence,
            entries,
            pending_objects: pending,
            asked,
            source_missing: HashSet::new(),
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

    pub(super) fn on_ack(
        &mut self,
        engine: &mut Engine,
        peer: DeviceId,
        ack: Ack,
    ) -> Result<(), EngineError> {
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

    pub(super) fn process_head(
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
}

/// `IndexRequest` for `space` to `peer`, starting after `after_sequence`.
pub(super) fn index_request(peer: DeviceId, space: SpaceId, after_sequence: u64) -> SyncOutput {
    SyncOutput::Send {
        peer,
        body: frame::Body::IndexRequest(IndexRequest {
            space_id: space_id_bytes(&space),
            after_sequence,
        }),
    }
}

/// Request `peer`'s index for `space` from the stored receive watermark.
pub(super) fn resume_index(
    engine: &Engine,
    peer: DeviceId,
    space: SpaceId,
    out: &mut dyn FnMut(SyncOutput),
) -> Result<(), EngineError> {
    let after = engine.db.repo().sync_progress(peer, space)?.received_seq;
    out(index_request(peer, space, after.0));
    Ok(())
}
