//! Receive-side mass-delete holds (D22).

use super::*;

impl Syncer {
    /// Re-run batches held for a mass delete in `space` after a decision
    /// (D22). Without this the held head waits for a reconnect.
    pub(crate) fn resume_held(
        &mut self,
        engine: &mut Engine,
        space: SpaceId,
        out: &mut dyn FnMut(SyncOutput),
    ) -> Result<Vec<SyncEvent>, EngineError> {
        let mut events = Vec::new();
        for peer in self.connected_peers() {
            self.process_head(engine, peer, space, out, &mut events)?;
        }
        self.flush_progress(&mut events, false);
        Ok(events)
    }

    pub(super) fn mass_delete_guard(
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
                Some(DeleteHoldDecision::Apply) => {}
                Some(DeleteHoldDecision::Restore) => restore_mounts.push(mount),
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

pub(super) enum MassDeleteAction {
    Proceed,
    Hold,
}

pub(super) fn live_tombstones(
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

pub(super) fn applied_tombstone_paths(
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
