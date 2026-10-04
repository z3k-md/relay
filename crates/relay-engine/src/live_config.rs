//! [`ConfigChange`]s applied on the running sync loop.
//!
//! [`ConfigQueue`] applies a change, replies to the caller, and hands back an
//! [`Applied`] for the loop's follow-ups. A join whose offer has not arrived
//! yet waits here: the device setting up a pair may be a third machine, so
//! the join request and the offer travel on different connections.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use relay_core::{ConfigApplied, ConfigChange, SpaceId};

use crate::Engine;
use crate::error::EngineError;
use crate::sync::{SyncEvent, SyncOutput, Syncer};

type Reply = mpsc::Sender<Result<ConfigApplied, ConfigRejected>>;

/// Why a change was refused, with [`EngineError::code`] kept across the
/// loop's reply channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigRejected {
    pub code: &'static str,
    pub message: String,
}

impl From<&EngineError> for ConfigRejected {
    fn from(err: &EngineError) -> Self {
        Self {
            code: err.code(),
            message: err.to_string(),
        }
    }
}

/// A change that took effect, for the loop's follow-ups.
pub(crate) struct Applied {
    pub change: ConfigChange,
    pub result: ConfigApplied,
    /// The space the change touched. Looked up before applying, so a deleted
    /// space still has its id here.
    pub space: Option<SpaceId>,
}

struct WaitingJoin {
    change: ConfigChange,
    reply: Reply,
    deadline: Instant,
}

#[derive(Default)]
pub(crate) struct ConfigQueue {
    waiting: Vec<WaitingJoin>,
}

impl ConfigQueue {
    /// Apply `change` and send its reply, unless it is a join still waiting
    /// for its offer.
    pub fn submit(
        &mut self,
        engine: &mut Engine,
        change: ConfigChange,
        reply: Reply,
    ) -> Option<Applied> {
        let space = space_id(engine, &change);
        let result = engine.apply_config(&change);
        if let (ConfigChange::JoinSpace { wait_ms, .. }, Err(EngineError::UnknownOffer(_))) =
            (&change, &result)
            && *wait_ms > 0
        {
            self.waiting.push(WaitingJoin {
                deadline: Instant::now() + Duration::from_millis(*wait_ms),
                change,
                reply,
            });
            return None;
        }
        finish(change, space, result, &reply)
    }

    /// Retry waiting joins. Call after applying peer frames.
    pub fn retry(&mut self, engine: &mut Engine) -> Vec<Applied> {
        let mut applied = Vec::new();
        for join in std::mem::take(&mut self.waiting) {
            match engine.apply_config(&join.change) {
                Err(EngineError::UnknownOffer(_)) => self.waiting.push(join),
                result => {
                    let space = space_id(engine, &join.change);
                    applied.extend(finish(join.change, space, result, &join.reply));
                }
            }
        }
        applied
    }

    /// Fail joins whose offer never came.
    pub fn expire(&mut self, now: Instant) {
        self.waiting.retain(|join| {
            if now < join.deadline {
                return true;
            }
            let space = join.change.space().unwrap_or_default().to_owned();
            let _ = join
                .reply
                .send(Err((&EngineError::UnknownOffer(space)).into()));
            false
        });
    }

    pub fn is_waiting(&self) -> bool {
        !self.waiting.is_empty()
    }
}

fn finish(
    change: ConfigChange,
    space: Option<SpaceId>,
    result: Result<ConfigApplied, EngineError>,
    reply: &Reply,
) -> Option<Applied> {
    match result {
        Ok(result) => {
            let _ = reply.send(Ok(result.clone()));
            let space = space.or(match &result {
                ConfigApplied::Space { space } => Some(space.id),
                ConfigApplied::Mount { mount, .. } => Some(mount.space),
                ConfigApplied::Peer { .. } | ConfigApplied::Holds { .. } | ConfigApplied::Done => {
                    None
                }
            });
            Some(Applied {
                change,
                result,
                space,
            })
        }
        Err(err) => {
            let _ = reply.send(Err((&err).into()));
            None
        }
    }
}

fn space_id(engine: &Engine, change: &ConfigChange) -> Option<SpaceId> {
    let name = change.space()?;
    engine
        .db
        .repo()
        .space_by_name(name)
        .ok()
        .flatten()
        .map(|space| space.id)
}

impl Syncer {
    /// Bring live sessions in line with an applied change: the trusted set,
    /// offers, index requests, which spaces each peer is sent, and held
    /// batches waiting on a decision.
    pub(crate) fn after_config(
        &mut self,
        engine: &mut Engine,
        applied: &Applied,
        out: &mut dyn FnMut(SyncOutput),
    ) -> Result<Vec<SyncEvent>, EngineError> {
        let space = applied.space;
        match &applied.change {
            ConfigChange::CreateSpace { .. }
            | ConfigChange::MaterializeAdd { .. }
            | ConfigChange::MaterializeRemove { .. }
            | ConfigChange::GroupCreate { .. } => {}
            ConfigChange::JoinSpace { .. } => {
                // Joining adopts the offer's members as peers (D26).
                out(SyncOutput::SetPeers);
                if let Some(space) = space {
                    self.refresh_offers_for_space(engine, space, out)?;
                    self.request_index(engine, space, out)?;
                }
            }
            ConfigChange::AddMount { .. } | ConfigChange::Share { .. } => {
                if let Some(space) = space {
                    self.refresh_offers_for_space(engine, space, out)?;
                    self.request_index(engine, space, out)?;
                }
            }
            ConfigChange::RemoveMount { .. }
            | ConfigChange::PolicyAdd { .. }
            | ConfigChange::PolicyRemove { .. } => {
                // Offers carry the mount list and the policy epoch (D27).
                if let Some(space) = space {
                    self.refresh_offers_for_space(engine, space, out)?;
                }
            }
            ConfigChange::Unshare { peer, .. } => {
                if let (Some(space), Some(peer)) = (space, engine.db.repo().peer_by_name(peer)?) {
                    self.stop_sending(peer.device.id, space);
                    self.refresh_offers(engine, peer.device.id, out)?;
                }
            }
            ConfigChange::DeleteSpace { .. } => {
                for peer in self.connected_peers() {
                    if let Some(space) = space {
                        self.stop_sending(peer, space);
                    }
                    self.refresh_offers(engine, peer, out)?;
                }
            }
            ConfigChange::SetPeerManage { .. } => {
                // The network layer holds the grant and tells the peer.
                out(SyncOutput::SetPeers);
            }
            ConfigChange::AddPeer { .. }
            | ConfigChange::RemovePeer { .. }
            | ConfigChange::RevokePeer { .. } => {
                // The network drops sessions of peers that left the trusted
                // set; members listed on everyone else's offers changed.
                out(SyncOutput::SetPeers);
                self.refresh_all_offers(engine, out)?;
            }
            ConfigChange::GroupAdd { .. }
            | ConfigChange::GroupRemove { .. }
            | ConfigChange::GroupDelete { .. } => {
                // Groups are expanded into every policy that names them.
                self.refresh_all_offers(engine, out)?;
            }
            ConfigChange::DecideDeleteHold { .. } => {
                if let Some(space) = space {
                    return self.resume_held(engine, space, out);
                }
            }
        }
        Ok(Vec::new())
    }

    fn refresh_all_offers(
        &self,
        engine: &Engine,
        out: &mut dyn FnMut(SyncOutput),
    ) -> Result<(), EngineError> {
        for peer in self.connected_peers() {
            self.refresh_offers(engine, peer, out)?;
        }
        Ok(())
    }
}
