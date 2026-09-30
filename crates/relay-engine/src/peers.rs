use std::collections::BTreeMap;

use relay_core::conflict::original_path;
use relay_core::{
    Device, DeviceId, EntryRecord, LogicalPath, Mount, Space, SpaceId, git_dir_of, validate_name,
};
use relay_db::{OfferedMount, PeerOfferRow, StoredOffer};
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
        Ok(PeerInfo {
            name: record.device.name,
            id: record.device.id,
            addresses: record.addresses,
            added_at_ms: record.added_at_ms,
        })
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
        Ok(self
            .db
            .repo()
            .list_peers()?
            .into_iter()
            .map(|p| PeerInfo {
                name: p.device.name,
                id: p.device.id,
                addresses: p.addresses,
                added_at_ms: p.added_at_ms,
            })
            .collect())
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

    pub fn offers(&self) -> Result<Vec<OfferInfo>, EngineError> {
        Ok(self
            .db
            .repo()
            .list_offers()?
            .iter()
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
        Ok(space)
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

    pub(crate) fn space_offers_for_peer(
        &self,
        peer: DeviceId,
    ) -> Result<relay_proto::SpaceOffers, EngineError> {
        let mut spaces = Vec::new();
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
            spaces.push(relay_proto::SpaceOffer {
                space_id: relay_proto::space_id_bytes(&space.id),
                name: space.name,
                mounts,
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
