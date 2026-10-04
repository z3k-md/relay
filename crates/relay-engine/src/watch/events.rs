//! Turning sync and mailbox results into [`WatchEvent`]s.

use super::*;

pub(super) fn emit_sync(
    result: Result<Vec<SyncEvent>, EngineError>,
    on_event: &mut dyn FnMut(&WatchEvent),
) {
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

pub(super) fn emit_pull(
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

pub(super) fn emit_push(
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
