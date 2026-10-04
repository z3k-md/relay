//! Per-space keys, mailbox sealing, recovery, and revocation (D30).

use std::collections::{BTreeMap, BTreeSet};

use relay_core::{DeviceId, ObjectId, SpaceId};
use relay_crypto::{
    format_recovery, open_object, open_recovery, parse_recovery, random_bytes, seal_object,
    seal_recovery, sealed_generation, verify_device, wrap_key,
};
use relay_db::SpaceKeyWrap;
use relay_replica::{DurableReplica, FsReplica, StoredKeyWrap};

use crate::Engine;
use crate::error::EngineError;

const PURPOSE_DEVICE: &str = "device";
const PURPOSE_RECOVERY: &str = "recovery";
const RECOVERY_SETTING: &str = "recovery_wrap";

pub(crate) enum MailboxRead {
    Missing,
    Ready(Vec<u8>),
    /// Ciphertext is present but this device cannot open it yet.
    Locked,
}

impl Engine {
    fn require_box(&self) -> Result<&relay_crypto::BoxKeyPair, EngineError> {
        self.box_key.as_ref().ok_or(EngineError::NoBoxKey)
    }

    fn recovery_recipient() -> DeviceId {
        DeviceId::from_bytes([0u8; 32])
    }

    /// Publish our box key, learn peers' keys, and import wraps addressed to us.
    pub(crate) fn prepare_mailbox(&mut self, replica: &FsReplica) -> Result<(), EngineError> {
        self.publish_box_key(replica)?;
        self.import_peer_boxes(replica)?;
        self.import_device_wraps(replica)?;
        Ok(())
    }

    pub(crate) fn publish_box_key(&self, replica: &FsReplica) -> Result<(), EngineError> {
        let box_key = self.require_box()?;
        let identity = self.load_identity()?;
        let public = box_key.public_bytes();
        let signature = identity.sign(&public)?;
        let mut body = Vec::with_capacity(96);
        body.extend_from_slice(&public);
        body.extend_from_slice(&signature);
        replica.put_box_key(self.device.id, &body)?;
        Ok(())
    }

    fn import_peer_boxes(&mut self, replica: &FsReplica) -> Result<(), EngineError> {
        let found = replica.list_box_keys()?;
        if found.is_empty() {
            return Ok(());
        }
        let me = self.device.id;
        self.db.transaction(|repo| {
            for (device, body) in &found {
                if *device == me || body.len() != 96 {
                    continue;
                }
                let mut public = [0u8; 32];
                public.copy_from_slice(&body[..32]);
                if !verify_device(device, &public, &body[32..]) {
                    tracing::warn!(%device, "ignoring mailbox box key with a bad signature");
                    continue;
                }
                repo.put_peer_box_key(*device, &public)?;
            }
            Ok::<(), EngineError>(())
        })?;
        Ok(())
    }

    fn import_device_wraps(&mut self, replica: &FsReplica) -> Result<(), EngineError> {
        let me = self.device.id;
        let box_key = self.require_box()?;
        let found = replica.list_key_wraps()?;
        let mut accepted = Vec::new();
        for StoredKeyWrap {
            space,
            generation,
            recipient,
            wrapped,
        } in found
        {
            if recipient != me {
                continue;
            }
            if box_key.unwrap(&wrapped).is_err() {
                continue;
            }
            accepted.push((space, generation, wrapped));
        }
        if accepted.is_empty() {
            return Ok(());
        }
        self.db.transaction(|repo| {
            for (space, generation, wrapped) in &accepted {
                repo.put_space_key_wrap(&SpaceKeyWrap {
                    space: *space,
                    generation: *generation,
                    purpose: PURPOSE_DEVICE.into(),
                    recipient: me,
                    wrapped: wrapped.clone(),
                })?;
            }
            Ok::<(), EngineError>(())
        })?;
        Ok(())
    }

    fn space_keys(&self, space: SpaceId) -> Result<BTreeMap<u32, [u8; 32]>, EngineError> {
        let box_key = self.require_box()?;
        let me = self.device.id;
        let mut out = BTreeMap::new();
        for wrap in self.db.repo().space_key_wraps(space)? {
            if wrap.purpose != PURPOSE_DEVICE || wrap.recipient != me {
                continue;
            }
            let key = box_key.unwrap(&wrap.wrapped)?;
            out.insert(wrap.generation, key);
        }
        Ok(out)
    }

    pub(crate) fn ensure_space_key(
        &mut self,
        space: SpaceId,
    ) -> Result<(u32, [u8; 32]), EngineError> {
        let existing = self.space_keys(space)?;
        if let Some((generation, key)) = existing.iter().next_back() {
            return Ok((*generation, *key));
        }
        let key = random_bytes();
        self.store_device_wrap(space, 1, &key)?;
        self.maybe_recovery_wrap(space, 1, &key)?;
        Ok((1, key))
    }

    fn store_device_wrap(
        &mut self,
        space: SpaceId,
        generation: u32,
        key: &[u8; 32],
    ) -> Result<(), EngineError> {
        let box_key = self.require_box()?;
        let wrapped = wrap_key(&box_key.public_bytes(), key)?;
        let me = self.device.id;
        self.db.transaction(|repo| {
            repo.put_space_key_wrap(&SpaceKeyWrap {
                space,
                generation,
                purpose: PURPOSE_DEVICE.into(),
                recipient: me,
                wrapped,
            })?;
            Ok::<(), EngineError>(())
        })?;
        Ok(())
    }

    fn maybe_recovery_wrap(
        &mut self,
        space: SpaceId,
        generation: u32,
        key: &[u8; 32],
    ) -> Result<(), EngineError> {
        let Some(secret) = self.recovery_secret()? else {
            return Ok(());
        };
        self.ensure_recovery_wrap(None, space, generation, key, &secret)
    }

    /// Keep one recovery wrap per generation. A later call reuses the stored
    /// bytes instead of sealing a new nonce on every sync.
    fn ensure_recovery_wrap(
        &mut self,
        replica: Option<&FsReplica>,
        space: SpaceId,
        generation: u32,
        key: &[u8; 32],
        secret: &[u8; 32],
    ) -> Result<(), EngineError> {
        let existing = self.db.repo().space_key_wraps(space)?;
        let wrapped = if let Some(found) = existing
            .iter()
            .find(|wrap| wrap.purpose == PURPOSE_RECOVERY && wrap.generation == generation)
        {
            found.wrapped.clone()
        } else {
            let wrapped = seal_recovery(secret, key)?;
            self.db.transaction(|repo| {
                repo.put_space_key_wrap(&SpaceKeyWrap {
                    space,
                    generation,
                    purpose: PURPOSE_RECOVERY.into(),
                    recipient: Self::recovery_recipient(),
                    wrapped: wrapped.clone(),
                })?;
                Ok::<(), EngineError>(())
            })?;
            wrapped
        };
        if let Some(replica) = replica {
            replica.put_recovery_wrap(space, generation, &wrapped)?;
        } else if let Some(path) = self.replica_path()? {
            let opened = crate::replica::open_replica(&path)?;
            opened.put_recovery_wrap(space, generation, &wrapped)?;
        }
        Ok(())
    }

    fn recovery_secret(&self) -> Result<Option<[u8; 32]>, EngineError> {
        let Some(stored) = self.db.repo().local_setting(RECOVERY_SETTING)? else {
            return Ok(None);
        };
        if stored.is_empty() {
            return Ok(None);
        }
        let wrapped = hex::decode(stored).map_err(|_| EngineError::RecoveryRejected)?;
        Ok(Some(self.require_box()?.unwrap(&wrapped)?))
    }

    /// Write wraps of the current space key for every peer whose box key we know.
    pub(crate) fn publish_space_wraps(
        &mut self,
        replica: &FsReplica,
        space: SpaceId,
    ) -> Result<(), EngineError> {
        let (generation, key) = self.ensure_space_key(space)?;
        let stored = self.db.repo().space_key_wraps(space)?;
        let peers = self.db.repo().peers_sharing_space(space)?;
        for peer in peers {
            if self.db.repo().device_status(peer)?.as_deref() == Some("revoked") {
                continue;
            }
            if let Some(existing) = stored.iter().find(|wrap| {
                wrap.purpose == PURPOSE_DEVICE
                    && wrap.generation == generation
                    && wrap.recipient == peer
            }) {
                replica.put_key_wrap(space, generation, peer, &existing.wrapped)?;
                continue;
            }
            let Some(public) = self.db.repo().peer_box_key(peer)? else {
                continue;
            };
            let wrapped = wrap_key(&public, &key)?;
            self.db.transaction(|repo| {
                repo.put_space_key_wrap(&SpaceKeyWrap {
                    space,
                    generation,
                    purpose: PURPOSE_DEVICE.into(),
                    recipient: peer,
                    wrapped: wrapped.clone(),
                })?;
                Ok::<(), EngineError>(())
            })?;
            replica.put_key_wrap(space, generation, peer, &wrapped)?;
        }
        if let Some(secret) = self.recovery_secret()? {
            self.ensure_recovery_wrap(Some(replica), space, generation, &key, &secret)?;
        }
        Ok(())
    }

    /// Seal `object` into the mailbox. Returns whether a new sealed object was written.
    pub(crate) fn put_space_object(
        &mut self,
        replica: &FsReplica,
        space: SpaceId,
        object: ObjectId,
    ) -> Result<bool, EngineError> {
        if replica.get_sealed_object(space, &object)?.is_some() {
            replica.remove_plaintext_object(&object)?;
            return Ok(false);
        }
        let (generation, key) = self.ensure_space_key(space)?;
        let plaintext = self.store.read(&object)?;
        let sealed = seal_object(&key, generation, &object, &plaintext)?;
        replica.put_sealed_object(space, &object, &sealed)?;
        replica.remove_plaintext_object(&object)?;
        Ok(true)
    }

    pub(crate) fn take_space_object(
        &self,
        replica: &FsReplica,
        space: SpaceId,
        object: ObjectId,
    ) -> Result<MailboxRead, EngineError> {
        if let Some(sealed) = replica.get_sealed_object(space, &object)? {
            let Some(generation) = sealed_generation(&sealed) else {
                return Ok(MailboxRead::Locked);
            };
            let keys = self.space_keys(space)?;
            let Some(key) = keys.get(&generation) else {
                return Ok(MailboxRead::Locked);
            };
            let plain = match open_object(key, &object, &sealed) {
                Ok(plain) => plain,
                Err(_) => return Ok(MailboxRead::Locked),
            };
            if ObjectId::of(&plain) != object {
                return Ok(MailboxRead::Locked);
            }
            return Ok(MailboxRead::Ready(plain));
        }
        match replica.get_object(&object)? {
            Some(bytes) => Ok(MailboxRead::Ready(bytes)),
            None => Ok(MailboxRead::Missing),
        }
    }

    /// Show the recovery secret, creating it on first call, and wrap every
    /// space key this device can already open.
    pub fn reveal_recovery_key(&mut self) -> Result<String, EngineError> {
        self.ensure_writable()?;
        let secret = if let Some(secret) = self.recovery_secret()? {
            secret
        } else {
            let secret = random_bytes();
            let public = self.require_box()?.public_bytes();
            let wrapped = wrap_key(&public, &secret)?;
            let encoded = hex::encode(wrapped);
            self.db
                .transaction(|repo| repo.set_local_setting(RECOVERY_SETTING, &encoded))
                .map_err(EngineError::from_db)?;
            secret
        };
        for space in self.db.repo().list_spaces()? {
            let keys = self.space_keys(space.id)?;
            for (generation, key) in keys {
                self.ensure_recovery_wrap(None, space.id, generation, &key, &secret)?;
            }
        }
        Ok(format_recovery(&secret))
    }

    /// Unwrap mailbox (and local) recovery wraps and re-wrap them for this device.
    /// Returns how many space-key generations were installed.
    pub fn import_recovery_key(&mut self, text: &str) -> Result<usize, EngineError> {
        self.ensure_writable()?;
        let secret = parse_recovery(text).map_err(|_| EngineError::RecoveryRejected)?;
        let mut installed: BTreeSet<(SpaceId, u32)> = BTreeSet::new();
        let spaces = self.db.repo().list_spaces()?;
        for space in &spaces {
            for wrap in self.db.repo().space_key_wraps(space.id)? {
                if wrap.purpose != PURPOSE_RECOVERY {
                    continue;
                }
                if let Ok(key) = open_recovery(&secret, &wrap.wrapped) {
                    self.store_device_wrap(space.id, wrap.generation, &key)?;
                    installed.insert((space.id, wrap.generation));
                }
            }
        }
        if let Some(path) = self.replica_path()? {
            let replica = crate::replica::open_replica(&path)?;
            for (space, generation, wrapped) in replica.list_recovery_wraps()? {
                if installed.contains(&(space, generation)) {
                    continue;
                }
                if let Ok(key) = open_recovery(&secret, &wrapped) {
                    self.store_device_wrap(space, generation, &key)?;
                    installed.insert((space, generation));
                }
            }
        }
        if installed.is_empty() {
            return Err(EngineError::RecoveryRejected);
        }
        Ok(installed.len())
    }

    /// New generation for future mailbox objects. Older generations still open.
    pub fn rotate_space_key(&mut self, name: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let space = self
            .db
            .repo()
            .space_by_name(name)?
            .ok_or_else(|| EngineError::UnknownSpace(name.to_owned()))?;
        let existing = self.space_keys(space.id)?;
        let generation = match existing.keys().next_back().copied() {
            None => 1,
            Some(current) => current
                .checked_add(1)
                .ok_or_else(|| EngineError::Replica("space key generation exhausted".into()))?,
        };
        let key = random_bytes();
        self.store_device_wrap(space.id, generation, &key)?;
        self.maybe_recovery_wrap(space.id, generation, &key)?;
        if let Some(path) = self.replica_path()? {
            let replica = crate::replica::open_replica(&path)?;
            self.prepare_mailbox(&replica)?;
            self.publish_space_wraps(&replica, space.id)?;
        }
        Ok(())
    }

    /// Soft revoke: stop sharing and drop the peer from the dial set. Already
    /// decrypted copies on that device stay readable. Call [`Self::rotate_space_key`]
    /// to keep future objects from using a key the revoked device might hold.
    pub fn revoke_peer(&mut self, name: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let peer = self
            .find_peer(name)?
            .ok_or_else(|| EngineError::UnknownPeer(name.to_owned()))?;
        let id = peer.device.id;
        let spaces = self.db.repo().list_spaces()?;
        self.db.transaction(|repo| {
            repo.set_device_status(id, "revoked")?;
            repo.set_peer_manage(id, false)?;
            for space in &spaces {
                repo.unshare_space(space.id, id)?;
            }
            Ok::<(), EngineError>(())
        })?;
        Ok(())
    }
}
