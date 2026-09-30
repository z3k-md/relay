//! Durable mailbox push/pull (D29).

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use relay_core::{EntryContent, MountId, ObjectId, Sequence};
use relay_proto::{RemoteEntry, entry_from_wire, entry_to_wire};
use relay_replica::{DurableReplica, FsReplica, ReplicaError, ReplicaMode};
use serde::Serialize;

use crate::Engine;
use crate::error::EngineError;

const REPLICA_PATH_KEY: &str = "replica_path";
const PUSH_BATCH: usize = 256;

#[derive(Clone, Debug, Default, Serialize)]
pub struct ReplicaPush {
    pub spaces: usize,
    pub entries: usize,
    pub objects: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ReplicaPull {
    pub peers: usize,
    pub entries: usize,
    pub objects: usize,
    pub written: usize,
    pub deleted: usize,
    pub conflicts: usize,
    pub skipped: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReplicaStatus {
    pub path: Option<PathBuf>,
    pub pushed: Vec<ReplicaSpacePush>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReplicaSpacePush {
    pub space: String,
    pub pushed_seq: u64,
}

impl Engine {
    pub fn replica_path(&self) -> Result<Option<PathBuf>, EngineError> {
        Ok(self
            .db
            .repo()
            .local_setting(REPLICA_PATH_KEY)?
            .filter(|p| !p.is_empty())
            .map(PathBuf::from))
    }

    pub fn set_replica_path(&mut self, path: &Path) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let canonical = if path.exists() {
            let meta = std::fs::metadata(path)?;
            if !meta.is_dir() {
                return Err(EngineError::PathNotADirectory(path.to_path_buf()));
            }
            dunce::canonicalize(path)?
        } else {
            std::fs::create_dir_all(path)?;
            dunce::canonicalize(path)?
        };
        // Ensure the replica layout exists.
        let _ = FsReplica::open(&canonical)?;
        self.db
            .transaction(|repo| {
                repo.set_local_setting(REPLICA_PATH_KEY, &canonical.to_string_lossy())
            })
            .map_err(EngineError::from_db)?;
        Ok(())
    }

    pub fn clear_replica_path(&mut self) -> Result<(), EngineError> {
        self.ensure_writable()?;
        self.db
            .transaction(|repo| repo.clear_local_setting(REPLICA_PATH_KEY))
            .map_err(EngineError::from_db)?;
        Ok(())
    }

    pub fn replica_status(&self) -> Result<ReplicaStatus, EngineError> {
        let path = self.replica_path()?;
        let mut pushed = Vec::new();
        for space in self.db.repo().list_spaces()? {
            let seq = self.db.repo().replica_pushed_seq(space.id)?;
            if seq.0 > 0 || path.is_some() {
                pushed.push(ReplicaSpacePush {
                    space: space.name,
                    pushed_seq: seq.0,
                });
            }
        }
        Ok(ReplicaStatus { path, pushed })
    }

    pub fn push_replica(&mut self) -> Result<ReplicaPush, EngineError> {
        self.ensure_writable()?;
        let Some(path) = self.replica_path()? else {
            return Ok(ReplicaPush::default());
        };
        let mut replica = open_replica(&path)?;
        let local = self.device().id;
        let spaces = self.db.repo().list_spaces()?;
        let mut report = ReplicaPush::default();

        for space in spaces {
            let mut pushed = self.db.repo().replica_pushed_seq(space.id)?;
            let mut advanced = false;
            loop {
                let changes = self
                    .db
                    .repo()
                    .changes_since_in_space(space.id, pushed, PUSH_BATCH)?;
                if changes.is_empty() {
                    break;
                }
                advanced = true;
                let mut wires = Vec::with_capacity(changes.len());
                let mut objects: Vec<ObjectId> = Vec::new();
                for entry in &changes {
                    if let EntryContent::File { object, .. } = &entry.content {
                        objects.push(*object);
                    }
                    wires.push(entry_to_wire(entry));
                }
                for object in &objects {
                    if replica.get_object(object)?.is_none() {
                        let bytes = self.store.read(object)?;
                        replica.put_object(*object, &bytes)?;
                        report.objects += 1;
                    }
                }
                replica.append_entries(local, space.id, &wires)?;
                report.entries += wires.len();
                pushed = changes.last().map(|e| e.sequence).unwrap_or(pushed);
                self.db
                    .transaction(|repo| repo.set_replica_pushed_seq(space.id, pushed))
                    .map_err(EngineError::from_db)?;
                if changes.len() < PUSH_BATCH {
                    break;
                }
            }
            if advanced {
                report.spaces += 1;
            }
        }
        Ok(report)
    }

    pub fn pull_replica(&mut self) -> Result<ReplicaPull, EngineError> {
        self.ensure_writable()?;
        let Some(path) = self.replica_path()? else {
            return Ok(ReplicaPull::default());
        };
        let mut replica = open_replica(&path)?;
        let local = self.device().id;
        let spaces = self.db.repo().list_spaces()?;
        let mut report = ReplicaPull::default();
        let mut peers_touched = HashSet::new();

        for space in spaces {
            let peers = self.db.repo().peers_sharing_space(space.id)?;
            let mounts = self.db.repo().list_mounts(Some(space.id))?;
            let mount_names: HashMap<MountId, String> = mounts
                .into_iter()
                .map(|cfg| (cfg.mount.id, cfg.mount.name))
                .collect();

            for peer in peers {
                let after = self.db.repo().sync_progress(peer, space.id)?.received_seq;
                let wires = replica.entries_after(peer, space.id, after.0)?;
                if wires.is_empty() {
                    continue;
                }
                peers_touched.insert(peer);

                let mut to_apply: Vec<RemoteEntry> = Vec::new();
                let mut through = after.0;
                let mut objects_fetched = 0usize;

                for wire in wires {
                    let seq = wire.sequence;
                    let entry = match entry_from_wire(space.id, wire) {
                        Ok(e) => e,
                        Err(err) => {
                            tracing::warn!(%peer, error = %err, "skipping bad replica entry");
                            break;
                        }
                    };

                    let wanted = match mount_names.get(&entry.key.mount) {
                        Some(name) => self.wants(space.id, local, name, entry.key.path.as_str())?,
                        None => true,
                    };
                    if !wanted {
                        through = seq;
                        continue;
                    }

                    if let Some(obj) = entry.content.object()
                        && !self.store.contains(&obj)
                    {
                        match replica.get_object(&obj)? {
                            Some(bytes) => {
                                self.store.put_bytes(&bytes)?;
                                objects_fetched += 1;
                            }
                            None => {
                                break;
                            }
                        }
                    }

                    to_apply.push(entry);
                    through = seq;
                }

                if !to_apply.is_empty() || through > after.0 {
                    let failed = HashSet::new();
                    if !to_apply.is_empty() {
                        let outcome =
                            self.apply_remote_batch(peer, space.id, to_apply.clone(), &failed)?;
                        report.written += outcome.written;
                        report.deleted += outcome.deleted;
                        report.conflicts += outcome.conflicts;
                        report.skipped += outcome.skipped;
                        report.entries += to_apply.len();
                        if outcome.transient {
                            // Some of the prefix was not written. Leave the
                            // cursor so the next pull retries the same range.
                            continue;
                        }
                    }
                    if through > after.0 {
                        let now = self.clock.now_ms();
                        self.db.transaction(|repo| {
                            repo.set_received_seq(peer, space.id, Sequence(through), now)
                        })?;
                        replica.put_ack(local, peer, space.id, through)?;
                    }
                }
                report.objects += objects_fetched;
            }
        }
        report.peers = peers_touched.len();
        Ok(report)
    }

    pub fn replica_gc(
        &mut self,
        mirror: bool,
        grace: Duration,
    ) -> Result<relay_replica::GcReport, EngineError> {
        self.ensure_writable()?;
        let path = self
            .replica_path()?
            .ok_or_else(|| EngineError::Replica("no replica path configured".into()))?;
        let mut replica = open_replica(&path)?;
        let mode = if mirror {
            ReplicaMode::Mirror
        } else {
            ReplicaMode::Mailbox
        };
        let local = self.device().id;
        let mut members = Vec::new();
        for space in self.db.repo().list_spaces()? {
            members.push((space.id, local));
            for peer in self.db.repo().peers_sharing_space(space.id)? {
                members.push((space.id, peer));
            }
        }
        Ok(replica.gc(mode, grace, &members)?)
    }

    /// Soft-fail wrapper for the watch loop: logs via the returned error string.
    pub(crate) fn push_replica_watch(&mut self) -> Result<ReplicaPush, EngineError> {
        self.push_replica()
    }

    pub(crate) fn pull_replica_watch(&mut self) -> Result<ReplicaPull, EngineError> {
        self.pull_replica()
    }
}

fn open_replica(path: &Path) -> Result<FsReplica, EngineError> {
    if !path.exists() || !path.is_dir() {
        return Err(EngineError::Replica(format!(
            "replica path is missing or not a directory: {}",
            path.display()
        )));
    }
    Ok(FsReplica::open(path)?)
}

impl From<ReplicaError> for EngineError {
    fn from(err: ReplicaError) -> Self {
        EngineError::Replica(err.to_string())
    }
}
