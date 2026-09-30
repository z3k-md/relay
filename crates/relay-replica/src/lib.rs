//! Durable non-materializing mailbox (D29).
//!
//! Stores content-addressed objects and length-prefixed [`WireEntry`] logs so a
//! device can catch up when its peer is offline. Does not reconstruct a
//! filesystem. Encryption at rest is a later phase.

mod error;
mod fs;

pub use error::ReplicaError;
pub use fs::FsReplica;

use std::time::Duration;

use relay_core::{DeviceId, ObjectId, SpaceId};
use relay_proto::WireEntry;
use serde::Serialize;

/// Mailbox vs long-lived mirror retention for [`DurableReplica::gc`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReplicaMode {
    /// Delete fully-acked entries (and their objects) after `grace`.
    #[default]
    Mailbox,
    /// Like mailbox, but keep the object of the latest live entry per path.
    Mirror,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GcReport {
    pub entries_removed: usize,
    pub objects_removed: usize,
    pub bytes_freed: u64,
}

/// Durable mailbox / mirror behind a local directory.
pub trait DurableReplica {
    fn put_object(&mut self, id: ObjectId, bytes: &[u8]) -> Result<(), ReplicaError>;
    fn get_object(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, ReplicaError>;
    /// Append this device's newly committed entries for one space.
    ///
    /// Idempotent on `(device, space, sequence)`: writing the same sequence again
    /// is a no-op if the payload matches, and errors if it differs. Sequences
    /// must be strictly increasing within the space log.
    fn append_entries(
        &mut self,
        device: DeviceId,
        space: SpaceId,
        entries: &[WireEntry],
    ) -> Result<(), ReplicaError>;
    fn entries_after(
        &self,
        device: DeviceId,
        space: SpaceId,
        after_sequence: u64,
    ) -> Result<Vec<WireEntry>, ReplicaError>;
    fn put_ack(
        &mut self,
        reader: DeviceId,
        author: DeviceId,
        space: SpaceId,
        through_sequence: u64,
    ) -> Result<(), ReplicaError>;
    fn ack(&self, reader: DeviceId, author: DeviceId, space: SpaceId) -> Result<u64, ReplicaError>;
    /// `members` is every device that shares each space, including this one.
    /// An entry is removable only after every other member has acked it.
    /// An empty member list deletes nothing.
    fn gc(
        &mut self,
        mode: ReplicaMode,
        grace: Duration,
        members: &[(SpaceId, DeviceId)],
    ) -> Result<GcReport, ReplicaError>;
}
