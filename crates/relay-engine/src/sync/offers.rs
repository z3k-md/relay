//! Space offers and the membership and policy changes they carry (D15, D26, D27).

use super::*;

impl Syncer {
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

    pub(super) fn on_offers(
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
        // Spaces this peer offered before. One it offers for the first time
        // and that this device already shares with it is one the peer just
        // joined: any index request sent earlier found nothing to answer.
        let known: HashSet<SpaceId> = engine
            .db
            .repo()
            .list_offers()?
            .into_iter()
            .filter(|offer| offer.peer.id == peer)
            .map(|offer| offer.space_id)
            .collect();
        let mut newly_joined: Vec<SpaceId> = Vec::new();
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
                if !known.contains(&space_id) && engine.db.repo().is_shared(space_id, peer)? {
                    newly_joined.push(space_id);
                }
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
        for space in newly_joined {
            resume_index(engine, peer, space, out)?;
        }
        for (member, space) in follow_up {
            self.refresh_offers(engine, member, out)?;
            let needs_index = self
                .connected
                .get(&member)
                .is_some_and(|c| !c.send_cursor.contains_key(&space));
            if needs_index {
                resume_index(engine, member, space, out)?;
            }
        }
        for space in policy_replays {
            if let Some(conn) = self.connected.get_mut(&peer) {
                conn.send_cursor.insert(space, Sequence::ZERO);
            }
            self.send_batches(engine, peer, space, true, out, events)?;
            out(index_request(peer, space, 0));
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
}
