//! Durable mailbox push/pull (D29).

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use relay_core::{EntryContent, MountId, ObjectId, Sequence, merge_peer_addresses};

use crate::secrets::MailboxRead;
use relay_proto::{RemoteEntry, entry_from_wire, entry_to_wire};
use relay_replica::{DurableReplica, FsReplica, ReplicaError, ReplicaMode};
use serde::Serialize;

use crate::Engine;
use crate::error::EngineError;

const REPLICA_PATH_KEY: &str = "replica_path";
const NAT_HINT: &str = "nat_hint";
const TRANSPORT_RELAY: &str = "transport_relay";
const TRANSPORT_SERVE: &str = "transport_serve";
const PUSH_BATCH: usize = 256;

#[derive(Clone, Debug, Default, Serialize)]
pub struct ReplicaPush {
    pub spaces: usize,
    pub entries: usize,
    pub objects: usize,
    /// Relay address adopted from the mailbox on this push.
    pub relay_adopted: Option<String>,
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
    /// Peer address lists changed because of mailbox NAT candidates.
    pub addresses_changed: bool,
    /// Relay address adopted from the mailbox on this pull.
    pub relay_adopted: Option<String>,
}

/// Relay address this device dials, and whether it also serves that port.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TransportStatus {
    pub relay: Option<String>,
    pub serve: bool,
    pub mailbox: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct NatExchange {
    pub addresses_changed: bool,
    pub relay_adopted: Option<String>,
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
        // Ensure the replica layout exists and publish our box key so a peer
        // can wrap space keys before the next push.
        let replica = FsReplica::open(&canonical)?;
        self.publish_box_key(&replica)?;
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

    /// Local sequences not yet appended to the mailbox.
    ///
    /// `None` when no mailbox is configured. `Some(0)` means every indexed
    /// change in each space has been appended. This is the sender's view:
    /// peers may still be behind on pull.
    pub fn replica_backlog(&self) -> Result<Option<u64>, EngineError> {
        if self.replica_path()?.is_none() {
            return Ok(None);
        }
        let mut behind = 0u64;
        for space in self.db.repo().list_spaces()? {
            let pushed = self.db.repo().replica_pushed_seq(space.id)?;
            let latest = self.db.repo().max_sequence_in_space(space.id)?;
            behind = behind.saturating_add(latest.0.saturating_sub(pushed.0));
        }
        Ok(Some(behind))
    }

    pub fn push_replica(&mut self) -> Result<ReplicaPush, EngineError> {
        self.ensure_writable()?;
        let Some(path) = self.replica_path()? else {
            return Ok(ReplicaPush::default());
        };
        let mut replica = open_replica(&path)?;
        self.prepare_mailbox(&replica)?;
        let exchanged = self.exchange_nat_on(&replica)?;
        let local = self.device().id;
        let spaces = self.db.repo().list_spaces()?;
        let mut report = ReplicaPush::default();

        for space in spaces {
            self.publish_space_wraps(&replica, space.id)?;
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
                    if self.put_space_object(&replica, space.id, *object)? {
                        report.objects += 1;
                    }
                }
                replica.append_entries(local, space.id, &wires)?;
                report.entries += wires.len();
                pushed = changes.last().map(|e| e.sequence).unwrap_or(pushed);
                // Crash window D29 describes: the mailbox has the entries, the
                // local watermark does not. `relay-sim` holds the process here
                // so a kill lands in that window. Unset in normal runs.
                stall_before_replica_watermark();
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
        report.relay_adopted = exchanged.relay_adopted;
        Ok(report)
    }

    pub fn pull_replica(&mut self) -> Result<ReplicaPull, EngineError> {
        self.ensure_writable()?;
        let Some(path) = self.replica_path()? else {
            return Ok(ReplicaPull::default());
        };
        let mut replica = open_replica(&path)?;
        self.prepare_mailbox(&replica)?;
        let exchanged = self.exchange_nat_on(&replica)?;
        let local = self.device().id;
        let spaces = self.db.repo().list_spaces()?;
        let mut report = ReplicaPull {
            addresses_changed: exchanged.addresses_changed,
            relay_adopted: exchanged.relay_adopted,
            ..ReplicaPull::default()
        };
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
                        match self.take_space_object(&replica, space.id, obj)? {
                            MailboxRead::Ready(bytes) => {
                                self.store.put_bytes(&bytes)?;
                                objects_fetched += 1;
                            }
                            MailboxRead::Missing | MailboxRead::Locked => break,
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

    pub fn set_nat_hint(&mut self, addrs: &[String]) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let value = addrs
            .iter()
            .filter(|addr| !addr.is_empty() && addr.len() <= 200 && !addr.contains('\n'))
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        self.db
            .transaction(|repo| repo.set_local_setting(NAT_HINT, &value))
            .map_err(EngineError::from_db)
    }

    /// Publish this device's NAT candidates and merge peers' candidates into
    /// their stored addresses. Returns whether any peer address list changed.
    pub fn exchange_nat(&mut self) -> Result<bool, EngineError> {
        Ok(self.exchange_nat_detail()?.addresses_changed)
    }

    pub(crate) fn exchange_nat_detail(&mut self) -> Result<NatExchange, EngineError> {
        self.ensure_writable()?;
        let Some(path) = self.replica_path()? else {
            return Ok(NatExchange::default());
        };
        let replica = open_replica(&path)?;
        self.exchange_nat_on(&replica)
    }

    pub fn transport_relay(&self) -> Result<Option<String>, EngineError> {
        Ok(self
            .db
            .repo()
            .local_setting(TRANSPORT_RELAY)?
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()))
    }

    pub fn transport_serve(&self) -> Result<bool, EngineError> {
        Ok(self.db.repo().local_setting(TRANSPORT_SERVE)?.as_deref() == Some("1"))
    }

    pub fn set_transport_relay(&mut self, addr: &str, serve: bool) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let addr = addr.trim();
        if !valid_transport_relay(addr) {
            return Err(EngineError::BadRelayAddress);
        }
        self.db
            .transaction(|repo| {
                repo.set_local_setting(TRANSPORT_RELAY, addr)?;
                if serve {
                    repo.set_local_setting(TRANSPORT_SERVE, "1")
                } else {
                    repo.clear_local_setting(TRANSPORT_SERVE)
                }
            })
            .map_err(EngineError::from_db)
    }

    pub fn clear_transport(&mut self) -> Result<(), EngineError> {
        self.ensure_writable()?;
        self.db
            .transaction(|repo| {
                repo.clear_local_setting(TRANSPORT_RELAY)?;
                repo.clear_local_setting(TRANSPORT_SERVE)
            })
            .map_err(EngineError::from_db)
    }

    pub fn transport_status(&self) -> Result<TransportStatus, EngineError> {
        let relay = self.transport_relay()?;
        let serve = self.transport_serve()?;
        let mailbox = match self.replica_path()? {
            Some(root) if root.is_dir() => FsReplica::read_transport_relay(&root)?,
            _ => None,
        };
        Ok(TransportStatus {
            relay,
            serve,
            mailbox,
        })
    }

    pub(crate) fn exchange_nat_on(
        &mut self,
        replica: &FsReplica,
    ) -> Result<NatExchange, EngineError> {
        if let Some(hint) = self.db.repo().local_setting(NAT_HINT)? {
            let addrs: Vec<String> = hint
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            if !addrs.is_empty() {
                replica.put_nat_candidates(self.device.id, &addrs)?;
            }
        }
        let found = replica.list_nat_candidates()?;
        let mut updates = Vec::new();
        for (device, addrs) in found {
            if device == self.device.id {
                continue;
            }
            let Some(peer) = self.db.repo().peer_by_id(device)? else {
                continue;
            };
            let merged = merge_peer_addresses(&peer.addresses, &addrs);
            if merged != peer.addresses {
                updates.push((device, merged));
            }
        }
        for (device, addrs) in &updates {
            self.set_peer_addresses(*device, addrs)?;
        }
        let relay_adopted = self.exchange_transport_relay(replica)?;
        Ok(NatExchange {
            addresses_changed: !updates.is_empty(),
            relay_adopted,
        })
    }

    fn exchange_transport_relay(
        &mut self,
        replica: &FsReplica,
    ) -> Result<Option<String>, EngineError> {
        let local = self.transport_relay()?;
        if let Some(addr) = local {
            replica.put_transport_relay(&addr)?;
            return Ok(None);
        }
        let Some(remote) = replica.transport_relay()? else {
            return Ok(None);
        };
        self.db
            .transaction(|repo| repo.set_local_setting(TRANSPORT_RELAY, &remote))
            .map_err(EngineError::from_db)?;
        Ok(Some(remote))
    }
}

fn valid_transport_relay(addr: &str) -> bool {
    if addr.is_empty() || addr.len() > 200 || addr.chars().any(char::is_whitespace) {
        return false;
    }
    if addr.parse::<std::net::SocketAddr>().is_ok() {
        return true;
    }
    let Some((host, port)) = addr.rsplit_once(':') else {
        return false;
    };
    !host.is_empty() && !host.contains(':') && port.parse::<u16>().is_ok()
}

/// `RELAY_SIM_STALL` is a file path set by `relay-sim` on its daemon
/// processes. When that file exists, a push that has already appended to the
/// mailbox waits before advancing the local watermark. The waiter writes
/// `<flag>.entered` and returns when the flag is removed, or after 60 seconds.
fn stall_before_replica_watermark() {
    let Ok(path) = std::env::var("RELAY_SIM_STALL") else {
        return;
    };
    let flag = PathBuf::from(path);
    if !flag.is_file() {
        return;
    }
    let entered = flag.with_extension("entered");
    let _ = std::fs::write(&entered, b"stalled\n");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while flag.is_file() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub(crate) fn open_replica(path: &Path) -> Result<FsReplica, EngineError> {
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
