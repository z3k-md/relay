use std::collections::{BTreeMap, HashSet};

use relay_core::conflict::original_path;
use relay_core::{
    Device, DeviceId, EntryRecord, LogicalPath, Mount, Space, SpaceId, git_dir_of,
    merge_peer_addresses, validate_name,
};
use relay_db::{OfferedMember, OfferedMount, PeerOfferRow, StoredOffer};
use serde::Serialize;

use crate::Engine;
use crate::error::EngineError;

/// How a live conflict copy is shown and grouped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConflictClass {
    File { original: LogicalPath },
    Git { git_dir: LogicalPath, is_ref: bool },
}

/// A live conflict copy plus its listing classification.
#[derive(Clone, Debug, Serialize)]
pub struct ConflictInfo {
    pub space: String,
    pub mount: String,
    pub class: ConflictClass,
    pub record: EntryRecord,
}

/// Classify a conflict-copy path as an ordinary file or Git metadata.
pub fn classify_conflict(path: &LogicalPath) -> ConflictClass {
    if let Some(git_dir) = git_dir_of(path)
        && path != &git_dir
    {
        let is_ref = git_dir
            .join("refs")
            .ok()
            .is_some_and(|refs| path.starts_with(&refs));
        return ConflictClass::Git { git_dir, is_ref };
    }
    ConflictClass::File {
        original: original_path(path).unwrap_or_else(|| path.clone()),
    }
}

/// Group Git conflict copies by `(space, mount, git_dir)`.
pub fn group_git_conflicts(
    items: &[ConflictInfo],
) -> BTreeMap<(String, String, LogicalPath), Vec<&ConflictInfo>> {
    let mut groups = BTreeMap::new();
    for item in items {
        if let ConflictClass::Git { git_dir, .. } = &item.class {
            groups
                .entry((item.space.clone(), item.mount.clone(), git_dir.clone()))
                .or_insert_with(Vec::new)
                .push(item);
        }
    }
    groups
}

#[derive(Clone, Debug, Serialize)]
pub struct PeerInfo {
    pub name: String,
    pub id: DeviceId,
    pub addresses: Vec<String>,
    pub added_at_ms: i64,
    /// Last live contact. `None` until this device has connected once.
    /// While the peer is offline, this is when that contact ended.
    pub last_seen_ms: Option<i64>,
    /// Soft-revoked devices stay listed but are not dialed or shared with.
    pub revoked: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct OfferInfo {
    pub peer: String,
    pub peer_id: DeviceId,
    pub space_id: SpaceId,
    pub name: String,
    pub mounts: Vec<OfferedMountInfo>,
    pub received_at_ms: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct OfferedMountInfo {
    pub id: relay_core::MountId,
    pub name: String,
}

impl From<&StoredOffer> for OfferInfo {
    fn from(offer: &StoredOffer) -> Self {
        Self {
            peer: offer.peer.name.clone(),
            peer_id: offer.peer.id,
            space_id: offer.space_id,
            name: offer.name.clone(),
            mounts: offer
                .mounts
                .iter()
                .map(|m| OfferedMountInfo {
                    id: m.id,
                    name: m.name.clone(),
                })
                .collect(),
            received_at_ms: offer.received_at_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdoptedMembers {
    /// True when a peer was added/updated (including address merges).
    pub peers_changed: bool,
    /// Peers newly shared for this space during adoption.
    pub newly_shared: Vec<DeviceId>,
}

impl Engine {
    pub fn add_peer(
        &mut self,
        name: &str,
        id: DeviceId,
        addresses: &[String],
    ) -> Result<PeerInfo, EngineError> {
        self.ensure_writable()?;
        validate_name(name)?;
        if id == self.device.id {
            return Err(EngineError::DuplicatePeer(name.to_owned()));
        }
        if self.db.repo().peer_by_name(name)?.is_some() {
            return Err(EngineError::DuplicatePeer(name.to_owned()));
        }
        if self.db.repo().peer_by_id(id)?.is_some() {
            return Err(EngineError::DuplicatePeer(name.to_owned()));
        }
        let device = Device {
            id,
            name: name.to_owned(),
        };
        let now = self.clock.now_ms();
        let record = self
            .db
            .transaction(|repo| repo.add_peer(&device, addresses, now))
            .map_err(EngineError::from_db)?;
        Ok(peer_info(record, false))
    }

    pub fn upsert_peer(
        &mut self,
        name: &str,
        id: DeviceId,
        addresses: &[String],
    ) -> Result<PeerInfo, EngineError> {
        self.ensure_writable()?;
        if id == self.device.id {
            return Err(EngineError::DuplicatePeer(name.to_owned()));
        }
        let name = self.unique_peer_name(name, id)?;
        validate_name(&name)?;
        let now = self.clock.now_ms();
        if self.db.repo().peer_by_id(id)?.is_some() {
            let record = self
                .db
                .transaction(|repo| repo.update_peer(id, &name, addresses, now))
                .map_err(EngineError::from_db)?;
            return Ok(peer_info(
                record,
                self.db.repo().device_status(id)?.as_deref() == Some("revoked"),
            ));
        }
        self.add_peer(&name, id, addresses)
    }

    fn unique_peer_name(&self, name: &str, id: DeviceId) -> Result<String, EngineError> {
        if let Some(existing) = self.db.repo().peer_by_name(name)?
            && existing.device.id != id
        {
            let candidate = format!("{name}-{}", id.short());
            validate_name(&candidate)?;
            return Ok(candidate);
        }
        Ok(name.to_owned())
    }

    pub fn set_peer_addresses(
        &mut self,
        id: DeviceId,
        addresses: &[String],
    ) -> Result<PeerInfo, EngineError> {
        self.ensure_writable()?;
        let Some(existing) = self.db.repo().peer_by_id(id)? else {
            return Err(EngineError::UnknownPeer(id.to_string()));
        };
        let now = self.clock.now_ms();
        let record = self
            .db
            .transaction(|repo| repo.update_peer(id, &existing.device.name, addresses, now))
            .map_err(EngineError::from_db)?;
        Ok(peer_info(
            record,
            self.db.repo().device_status(id)?.as_deref() == Some("revoked"),
        ))
    }

    pub fn share_space_id(&mut self, space: SpaceId, peer: DeviceId) -> Result<(), EngineError> {
        self.ensure_writable()?;
        if self.db.repo().space(space)?.is_none() {
            return Err(EngineError::UnknownSpace(space.to_string()));
        }
        if self.db.repo().peer_by_id(peer)?.is_none() {
            return Err(EngineError::UnknownPeer(peer.to_string()));
        }
        self.db
            .transaction(|repo| repo.share_space(space, peer))
            .map_err(EngineError::from_db)
    }

    pub fn remove_peer(&mut self, name: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let removed = self
            .db
            .transaction(|repo| repo.remove_peer_by_name(name))
            .map_err(EngineError::from_db)?;
        if !removed {
            return Err(EngineError::UnknownPeer(name.to_owned()));
        }
        Ok(())
    }

    pub fn peers(&self) -> Result<Vec<PeerInfo>, EngineError> {
        let mut out = Vec::new();
        for p in self.db.repo().list_peers()? {
            let revoked = self.db.repo().device_status(p.device.id)?.as_deref() == Some("revoked");
            out.push(peer_info(p, revoked));
        }
        Ok(out)
    }

    pub fn share(&mut self, space: &str, peer: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let space_rec = self
            .db
            .repo()
            .space_by_name(space)?
            .ok_or_else(|| EngineError::UnknownSpace(space.to_owned()))?;
        let peer_rec = self
            .db
            .repo()
            .peer_by_name(peer)?
            .ok_or_else(|| EngineError::UnknownPeer(peer.to_owned()))?;
        if self.db.repo().device_status(peer_rec.device.id)?.as_deref() == Some("revoked") {
            return Err(EngineError::PeerRevoked(peer.to_owned()));
        }
        self.db
            .transaction(|repo| repo.share_space(space_rec.id, peer_rec.device.id))
            .map_err(EngineError::from_db)?;
        Ok(())
    }

    pub fn unshare(&mut self, space: &str, peer: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let space_rec = self
            .db
            .repo()
            .space_by_name(space)?
            .ok_or_else(|| EngineError::UnknownSpace(space.to_owned()))?;
        let peer_rec = self
            .db
            .repo()
            .peer_by_name(peer)?
            .ok_or_else(|| EngineError::UnknownPeer(peer.to_owned()))?;
        self.db
            .transaction(|repo| repo.unshare_space(space_rec.id, peer_rec.device.id))
            .map_err(EngineError::from_db)?;
        Ok(())
    }

    /// Offers this device can still join. A space that already exists locally
    /// is omitted; the stored offer stays, so it shows again if that space is
    /// removed.
    pub fn offers(&self) -> Result<Vec<OfferInfo>, EngineError> {
        let joined: HashSet<_> = self
            .db
            .repo()
            .list_spaces()?
            .into_iter()
            .map(|space| space.id)
            .collect();
        Ok(self
            .db
            .repo()
            .list_offers()?
            .iter()
            .filter(|offer| !joined.contains(&offer.space_id))
            .map(OfferInfo::from)
            .collect())
    }

    pub fn join_space(&mut self, name_or_id: &str, from_peer: &str) -> Result<Space, EngineError> {
        self.ensure_writable()?;
        let peer = self
            .db
            .repo()
            .peer_by_name(from_peer)?
            .ok_or_else(|| EngineError::UnknownPeer(from_peer.to_owned()))?;
        let offer = self
            .db
            .repo()
            .offer_from_peer(peer.device.id, name_or_id)?
            .ok_or_else(|| EngineError::UnknownOffer(name_or_id.to_owned()))?;

        if let Some(existing) = self.db.repo().space_by_name(&offer.name)?
            && existing.id != offer.space_id
        {
            return Err(EngineError::SpaceIdConflict {
                name: offer.name.clone(),
            });
        }

        let now = self.clock.now_ms();
        let space = Space {
            id: offer.space_id,
            name: offer.name.clone(),
        };
        self.db
            .transaction(|repo| {
                if repo.space(offer.space_id)?.is_none() {
                    repo.create_space(&space, now)?;
                }
                for mount in &offer.mounts {
                    if repo.mount_by_name(offer.space_id, &mount.name)?.is_none() {
                        repo.create_mount(
                            &Mount {
                                id: mount.id,
                                space: offer.space_id,
                                name: mount.name.clone(),
                            },
                            now,
                        )?;
                    }
                }
                repo.share_space(offer.space_id, peer.device.id)?;
                Ok::<(), EngineError>(())
            })
            .map_err(|err| match err {
                EngineError::Db(inner) => EngineError::from_db(inner),
                other => other,
            })?;
        let _ = self.adopt_offered_members(offer.space_id, &offer.members)?;
        Ok(space)
    }

    /// Trust members listed on an offer for a space this device has already
    /// joined: upsert peers, merge addresses, and share the space. Dismissed
    /// peers and the local device are skipped.
    pub fn adopt_offered_members(
        &mut self,
        space: SpaceId,
        members: &[OfferedMember],
    ) -> Result<AdoptedMembers, EngineError> {
        self.ensure_writable()?;
        if self.db.repo().space(space)?.is_none() {
            return Err(EngineError::UnknownSpace(space.to_string()));
        }
        let local = self.device.id;
        let mut peers_changed = false;
        let mut newly_shared = Vec::new();
        for member in members {
            if member.id == local {
                continue;
            }
            if self.db.repo().is_dismissed(member.id)? {
                continue;
            }
            let existing = self.db.repo().peer_by_id(member.id)?;
            if let Some(peer) = existing {
                let merged = merge_peer_addresses(&member.addresses, &peer.addresses);
                if merged != peer.addresses {
                    self.set_peer_addresses(member.id, &merged)?;
                    peers_changed = true;
                }
            } else {
                let addresses = merge_peer_addresses(&[], &member.addresses);
                self.upsert_peer(&member.name, member.id, &addresses)?;
                peers_changed = true;
            }
            if !self.db.repo().is_shared(space, member.id)? {
                self.share_space_id(space, member.id)?;
                newly_shared.push(member.id);
                peers_changed = true;
            }
        }
        Ok(AdoptedMembers {
            peers_changed,
            newly_shared,
        })
    }

    pub fn conflicts(&self, space: Option<&str>) -> Result<Vec<EntryRecord>, EngineError> {
        Ok(self
            .conflict_infos(space)?
            .into_iter()
            .map(|c| c.record)
            .collect())
    }

    /// Live conflict copies with a file-vs-Git classification (D21).
    pub fn conflict_infos(&self, space: Option<&str>) -> Result<Vec<ConflictInfo>, EngineError> {
        let listed = self.mounts(space)?;
        let mut out = Vec::new();
        for (space_rec, config) in listed {
            for entry in self.db.repo().entries_for_mount(config.mount.id)? {
                if !entry.is_deleted() && relay_core::conflict::is_conflict_copy(&entry.key.path) {
                    let class = classify_conflict(&entry.key.path);
                    out.push(ConflictInfo {
                        space: space_rec.name.clone(),
                        mount: config.mount.name.clone(),
                        class,
                        record: entry,
                    });
                }
            }
        }
        out.sort_by(|a, b| {
            a.space
                .cmp(&b.space)
                .then_with(|| a.mount.cmp(&b.mount))
                .then_with(|| a.record.key.path.cmp(&b.record.key.path))
        });
        Ok(out)
    }

    pub(crate) fn persist_offers(
        &mut self,
        peer: DeviceId,
        offers: &[PeerOfferRow],
    ) -> Result<Vec<OfferInfo>, EngineError> {
        let now = self.clock.now_ms();
        self.db
            .transaction(|repo| repo.replace_peer_offers(peer, offers, now))
            .map_err(EngineError::from_db)?;
        Ok(self
            .db
            .repo()
            .list_offers()?
            .into_iter()
            .filter(|o| o.peer.id == peer)
            .map(|o| OfferInfo::from(&o))
            .collect())
    }

    pub fn space_offers_for_peer(
        &self,
        peer: DeviceId,
    ) -> Result<relay_proto::SpaceOffers, EngineError> {
        let mut spaces = Vec::new();
        let all_peers = self.db.repo().list_peers()?;
        for space_id in self.db.repo().shared_space_ids(peer)? {
            let Some(space) = self.db.repo().space(space_id)? else {
                continue;
            };
            let mounts = self
                .db
                .repo()
                .list_mounts(Some(space_id))?
                .into_iter()
                .map(|cfg| relay_proto::MountOffer {
                    mount_id: relay_proto::mount_id_bytes(&cfg.mount.id),
                    name: cfg.mount.name,
                })
                .collect();
            let mut members = Vec::new();
            for other in &all_peers {
                if other.device.id == peer || other.device.id == self.device.id {
                    continue;
                }
                if !self.db.repo().is_shared(space_id, other.device.id)? {
                    continue;
                }
                members.push(relay_proto::MemberOffer {
                    device_id: other.device.id.as_bytes().to_vec(),
                    name: other.device.name.clone(),
                    addresses: other.addresses.clone(),
                });
            }
            let (policy_epoch, policies) = self.local_policy_offers(space_id)?;
            spaces.push(relay_proto::SpaceOffer {
                space_id: relay_proto::space_id_bytes(&space.id),
                name: space.name,
                mounts,
                members,
                policy_epoch,
                policies,
            });
        }
        Ok(relay_proto::SpaceOffers { spaces })
    }

    pub(crate) fn record_peer_name(&mut self, id: DeviceId, name: &str) -> Result<(), EngineError> {
        let now = self.clock.now_ms();
        self.db
            .transaction(|repo| {
                repo.upsert_device(
                    &Device {
                        id,
                        name: name.to_owned(),
                    },
                    now,
                )
            })
            .map_err(EngineError::from_db)
    }

    /// Stamp `devices.last_seen_ms` for a peer we are talking to, or just lost.
    pub(crate) fn note_peer_seen(&mut self, id: DeviceId) -> Result<(), EngineError> {
        let now = self.clock.now_ms();
        self.db
            .transaction(|repo| repo.touch_last_seen(id, now))
            .map_err(EngineError::from_db)
    }
}

fn peer_info(record: relay_db::PeerRecord, revoked: bool) -> PeerInfo {
    PeerInfo {
        name: record.device.name,
        id: record.device.id,
        addresses: record.addresses,
        added_at_ms: record.added_at_ms,
        last_seen_ms: record.last_seen_ms,
        revoked,
    }
}

#[allow(dead_code)]
pub(crate) fn offered_mounts_from_wire(
    mounts: &[relay_proto::MountOffer],
) -> Result<Vec<OfferedMount>, EngineError> {
    let mut out = Vec::new();
    for m in mounts {
        out.push(OfferedMount {
            id: relay_proto::mount_id_from_bytes(&m.mount_id)?,
            name: m.name.clone(),
        });
    }
    Ok(out)
}
