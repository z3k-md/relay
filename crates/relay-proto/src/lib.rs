//! Relay peer wire protocol.
//!
//! Messages are protobuf (via `prost` derives, so no `protoc` is needed at
//! build time). Unknown fields are ignored by prost, which is what lets newer
//! peers add fields without breaking older ones; a breaking change bumps
//! [`PROTOCOL_VERSION`].
//!
//! # Streams
//!
//! Each authenticated QUIC connection carries:
//!
//! - One **control stream** (bidirectional, opened by the dialing side). Both
//!   directions carry length-prefixed [`Frame`]s. The first frame each way is
//!   [`Hello`].
//! - Any number of **object streams** (bidirectional, opened by whichever side
//!   wants bytes). The requester writes one [`ObjectRequest`] frame and
//!   finishes its send side. The responder writes one [`ObjectHeader`] frame,
//!   then exactly `size` raw bytes, then finishes.
//!
//! # Index sync
//!
//! For each space both sides share, each side sends an [`IndexRequest`] asking
//! for the peer's changes after the last sequence it has durably applied. The
//! peer answers with [`IndexBatch`]es and keeps streaming new batches as its
//! index changes. After applying a batch the receiver sends [`Ack`], which the
//! sender records as "peer has everything through N".

use relay_core::entry::EntryKey;
use relay_core::{
    DeviceId, EntryContent, EntryRecord, LogicalPath, MountId, ObjectId, PolicyId, Sequence,
    SpaceId, VersionVector,
};

/// Bumped on incompatible changes. Peers with different versions refuse to
/// sync and say so in their `Error` frame.
pub const PROTOCOL_VERSION: u32 = 1;

/// ALPN protocol id negotiated on every sync QUIC connection.
pub const ALPN: &[u8] = b"relay/1";

/// ALPN for the pairing handshake. Same UDP port as sync.
pub const PAIR_ALPN: &[u8] = b"relay-pair/1";

pub const PAIR_CONFIRM_B: &[u8] = b"relay-pair/1 confirm B";
pub const PAIR_CONFIRM_A: &[u8] = b"relay-pair/1 confirm A";
pub const PAIR_ID_INITIATOR: &[u8] = b"relay-pair-initiator";
pub const PAIR_ID_JOINER: &[u8] = b"relay-pair-joiner";

/// Upper bound on one encoded control frame. A batch never approaches this;
/// senders split large index transfers into many batches.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Entries per [`IndexBatch`] when sending a large backlog.
pub const INDEX_BATCH_ENTRIES: usize = 1_000;

#[derive(Clone, PartialEq, prost::Message)]
pub struct Frame {
    #[prost(oneof = "frame::Body", tags = "1, 2, 3, 4, 5, 6, 7, 8")]
    pub body: Option<frame::Body>,
}

pub mod frame {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Body {
        #[prost(message, tag = "1")]
        Hello(super::Hello),
        #[prost(message, tag = "2")]
        SpaceOffers(super::SpaceOffers),
        #[prost(message, tag = "3")]
        IndexRequest(super::IndexRequest),
        #[prost(message, tag = "4")]
        IndexBatch(super::IndexBatch),
        #[prost(message, tag = "5")]
        Ack(super::Ack),
        #[prost(message, tag = "6")]
        Ping(super::Ping),
        #[prost(message, tag = "7")]
        Pong(super::Ping),
        #[prost(message, tag = "8")]
        Error(super::ErrorFrame),
    }
}

impl Frame {
    pub fn new(body: frame::Body) -> Frame {
        Frame { body: Some(body) }
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Hello {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    /// Must equal the id derived from the peer's TLS certificate; checked by
    /// the receiver as a consistency guard.
    #[prost(bytes = "vec", tag = "2")]
    pub device_id: Vec<u8>,
    #[prost(string, tag = "3")]
    pub device_name: String,
    #[prost(string, tag = "4")]
    pub client_version: String,
}

/// Spaces the sender shares with the receiver. Sent after `Hello` and again
/// whenever the set changes. Receiving an offer never joins a space. Members
/// listed on an offer for a space this device has already joined are trusted
/// (see D26); membership alone does not join anything.
#[derive(Clone, PartialEq, prost::Message)]
pub struct SpaceOffers {
    #[prost(message, repeated, tag = "1")]
    pub spaces: Vec<SpaceOffer>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SpaceOffer {
    #[prost(bytes = "vec", tag = "1")]
    pub space_id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(message, repeated, tag = "3")]
    pub mounts: Vec<MountOffer>,
    /// Other devices this space is also shared with (excluding the recipient).
    #[prost(message, repeated, tag = "4")]
    pub members: Vec<MemberOffer>,
    /// Local policy epoch for this space (D27). Peers replay from sequence 0
    /// when the epoch they stored for us changes.
    #[prost(uint64, tag = "5")]
    pub policy_epoch: u64,
    /// Local policies for this space with groups already expanded to device ids.
    #[prost(message, repeated, tag = "6")]
    pub policies: Vec<PolicyOffer>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PolicyOffer {
    #[prost(bytes = "vec", tag = "1")]
    pub id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(string, repeated, tag = "3")]
    pub selectors: Vec<String>,
    /// Device ids; groups are expanded before the offer is built.
    #[prost(bytes = "vec", repeated, tag = "4")]
    pub targets: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct MemberOffer {
    #[prost(bytes = "vec", tag = "1")]
    pub device_id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(string, repeated, tag = "3")]
    pub addresses: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct MountOffer {
    #[prost(bytes = "vec", tag = "1")]
    pub mount_id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub name: String,
}

/// "Send me your changes to this space after `after_sequence`, then keep
/// streaming." Sequences are the *responder's* local sequence numbers.
#[derive(Clone, PartialEq, prost::Message)]
pub struct IndexRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub space_id: Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub after_sequence: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct IndexBatch {
    #[prost(bytes = "vec", tag = "1")]
    pub space_id: Vec<u8>,
    #[prost(message, repeated, tag = "2")]
    pub entries: Vec<WireEntry>,
    /// Every change to this space with sequence <= this value has now been
    /// sent. May exceed the highest entry sequence in the batch (changes to
    /// other spaces also consume sequence numbers).
    #[prost(uint64, tag = "3")]
    pub through_sequence: u64,
    /// True when the sender had nothing newer at the time it built the batch.
    #[prost(bool, tag = "4")]
    pub caught_up: bool,
    /// The batch covers the sender's changes in `(after_sequence,
    /// through_sequence]`. Lets a receiver tell a re-sent range from newer
    /// changes.
    #[prost(uint64, tag = "5")]
    pub after_sequence: u64,
    /// Entry count for the whole catch-up that started at `plan_after`, not
    /// just this batch. Absent when the sender has no backlog, or on older
    /// peers. Directories and deletes are included.
    #[prost(uint64, optional, tag = "6")]
    pub plan_files: Option<u64>,
    /// Sum of file sizes for that same catch-up. Directories and deletes
    /// contribute nothing. Absent alongside `plan_files`.
    #[prost(uint64, optional, tag = "7")]
    pub plan_bytes: Option<u64>,
    /// Sender sequence the catch-up plan starts after. Stamped on every batch
    /// of that plan so the receiver can tell a continuation from a new range.
    #[prost(uint64, optional, tag = "8")]
    pub plan_after: Option<u64>,
}

/// "I have durably applied your changes to this space through
/// `through_sequence`."
#[derive(Clone, PartialEq, prost::Message)]
pub struct Ack {
    #[prost(bytes = "vec", tag = "1")]
    pub space_id: Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub through_sequence: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Ping {
    #[prost(uint64, tag = "1")]
    pub nonce: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ErrorFrame {
    #[prost(string, tag = "1")]
    pub code: String,
    #[prost(string, tag = "2")]
    pub message: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireEntry {
    #[prost(bytes = "vec", tag = "1")]
    pub mount_id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub path: String,
    #[prost(oneof = "wire_entry::Content", tags = "3, 4, 5, 6")]
    pub content: Option<wire_entry::Content>,
    #[prost(message, repeated, tag = "7")]
    pub vector: Vec<Counter>,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub parent_object: Option<Vec<u8>>,
    #[prost(bytes = "vec", tag = "9")]
    pub modified_by: Vec<u8>,
    #[prost(int64, tag = "10")]
    pub modified_at_unix_ms: i64,
    /// The sender's local sequence for this version.
    #[prost(uint64, tag = "11")]
    pub sequence: u64,
    /// Sender's observed file mtime (Unix nanoseconds). Used so Git's stat
    /// checks on the other device keep matching. Absent for directories and
    /// tombstones, and for older peers that do not send it.
    #[prost(int64, optional, tag = "12")]
    pub mtime_unix_ns: Option<i64>,
}

pub mod wire_entry {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Content {
        #[prost(message, tag = "3")]
        File(super::WireFile),
        #[prost(message, tag = "4")]
        Directory(super::Empty),
        #[prost(string, tag = "5")]
        Symlink(String),
        #[prost(message, tag = "6")]
        Deleted(super::Empty),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireFile {
    #[prost(bytes = "vec", tag = "1")]
    pub object: Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub size: u64,
    #[prost(bool, tag = "3")]
    pub executable: bool,
}

#[derive(Clone, Copy, PartialEq, prost::Message)]
pub struct Empty {}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Counter {
    #[prost(bytes = "vec", tag = "1")]
    pub device_id: Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub value: u64,
}

/// First (and only) frame the requester writes on an object stream.
/// One length-prefixed pairing message on the pairing stream.
#[derive(Clone, PartialEq, prost::Message)]
pub struct PairingMessage {
    #[prost(oneof = "pairing_message::Body", tags = "1, 2, 3, 4, 5")]
    pub body: Option<pairing_message::Body>,
}

pub mod pairing_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Body {
        #[prost(message, tag = "1")]
        Join(super::PairJoin),
        #[prost(message, tag = "2")]
        Start(super::PairStart),
        #[prost(message, tag = "3")]
        ConfirmB(super::PairConfirm),
        #[prost(message, tag = "4")]
        ConfirmA(super::PairConfirmA),
        #[prost(message, tag = "5")]
        JoinInfo(super::PairDeviceInfo),
    }
}

impl PairingMessage {
    pub fn new(body: pairing_message::Body) -> Self {
        Self { body: Some(body) }
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PairJoin {
    #[prost(string, tag = "1")]
    pub nameplate: String,
    #[prost(bytes = "vec", tag = "2")]
    pub spake2_b: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PairStart {
    #[prost(bytes = "vec", tag = "1")]
    pub spake2_a: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PairConfirm {
    #[prost(bytes = "vec", tag = "1")]
    pub mac: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PairConfirmA {
    #[prost(bytes = "vec", tag = "1")]
    pub mac: Vec<u8>,
    #[prost(message, tag = "2")]
    pub info: Option<PairDeviceInfo>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PairDeviceInfo {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, repeated, tag = "2")]
    pub addresses: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ObjectRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub object_id: Vec<u8>,
}

/// First frame the responder writes on an object stream. When `found`, exactly
/// `size` raw bytes follow. The requester must verify the BLAKE3 hash before
/// using them.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ObjectHeader {
    #[prost(bool, tag = "1")]
    pub found: bool,
    #[prost(uint64, tag = "2")]
    pub size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_BYTES} byte limit")]
    FrameTooLarge(usize),
    #[error("malformed message: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("invalid field {field}: {reason}")]
    Invalid { field: &'static str, reason: String },
}

fn invalid(field: &'static str, reason: impl Into<String>) -> ProtoError {
    ProtoError::Invalid {
        field,
        reason: reason.into(),
    }
}

/// Encode a message as `u32 big-endian length || protobuf bytes`.
pub fn encode_frame<M: prost::Message>(msg: &M) -> Result<Vec<u8>, ProtoError> {
    let len = msg.encoded_len();
    if len > MAX_FRAME_BYTES {
        return Err(ProtoError::FrameTooLarge(len));
    }
    let mut out = Vec::with_capacity(4 + len);
    out.extend_from_slice(&u32::try_from(len).expect("bounded above").to_be_bytes());
    msg.encode(&mut out).expect("vec grows as needed");
    Ok(out)
}

/// Parse a length prefix, rejecting oversized frames before allocating.
pub fn frame_len(prefix: [u8; 4]) -> Result<usize, ProtoError> {
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(ProtoError::FrameTooLarge(len));
    }
    Ok(len)
}

pub fn decode_message<M: prost::Message + Default>(body: &[u8]) -> Result<M, ProtoError> {
    Ok(M::decode(body)?)
}

pub fn device_id_from_bytes(bytes: &[u8]) -> Result<DeviceId, ProtoError> {
    let arr: [u8; 32] = bytes.try_into().map_err(|_| {
        invalid(
            "device_id",
            format!("expected 32 bytes, got {}", bytes.len()),
        )
    })?;
    Ok(DeviceId::from_bytes(arr))
}

pub fn object_id_from_bytes(bytes: &[u8]) -> Result<ObjectId, ProtoError> {
    let arr: [u8; 32] = bytes.try_into().map_err(|_| {
        invalid(
            "object_id",
            format!("expected 32 bytes, got {}", bytes.len()),
        )
    })?;
    Ok(ObjectId::from_bytes(arr))
}

fn uuid_from_bytes(field: &'static str, bytes: &[u8]) -> Result<uuid::Uuid, ProtoError> {
    uuid::Uuid::from_slice(bytes).map_err(|e| invalid(field, e.to_string()))
}

pub fn space_id_from_bytes(bytes: &[u8]) -> Result<SpaceId, ProtoError> {
    uuid_from_bytes("space_id", bytes).map(SpaceId::from_uuid)
}

pub fn mount_id_from_bytes(bytes: &[u8]) -> Result<MountId, ProtoError> {
    uuid_from_bytes("mount_id", bytes).map(MountId::from_uuid)
}

pub fn policy_id_from_bytes(bytes: &[u8]) -> Result<PolicyId, ProtoError> {
    uuid_from_bytes("policy_id", bytes).map(PolicyId::from_uuid)
}

pub fn space_id_bytes(id: &SpaceId) -> Vec<u8> {
    id.as_uuid().as_bytes().to_vec()
}

pub fn mount_id_bytes(id: &MountId) -> Vec<u8> {
    id.as_uuid().as_bytes().to_vec()
}

pub fn policy_id_bytes(id: &PolicyId) -> Vec<u8> {
    id.as_uuid().as_bytes().to_vec()
}

/// A replicated entry as received from a peer. `sequence` is the peer's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEntry {
    pub key: EntryKey,
    pub content: EntryContent,
    pub vector: VersionVector,
    pub parent_object: Option<ObjectId>,
    pub modified_by: DeviceId,
    pub modified_at_unix_ms: i64,
    pub sequence: Sequence,
    pub mtime_ns: Option<i64>,
}

impl RemoteEntry {
    /// Convert to a local index record. `sequence` must be a fresh local
    /// sequence number and `stat` whatever was observed after materializing.
    pub fn into_record(
        self,
        sequence: Sequence,
        stat: Option<relay_core::StatHint>,
    ) -> EntryRecord {
        EntryRecord {
            key: self.key,
            content: self.content,
            vector: self.vector,
            parent_object: self.parent_object,
            sequence,
            modified_by: self.modified_by,
            modified_at_unix_ms: self.modified_at_unix_ms,
            stat, // mtime_ns is applied at materialize time; stored stat is observed after write
        }
    }
}

/// Wire form of a local record. Stat hints are device-local and never sent.
pub fn entry_to_wire(record: &EntryRecord) -> WireEntry {
    use wire_entry::Content;
    let content = match &record.content {
        EntryContent::File {
            object,
            size,
            executable,
        } => Content::File(WireFile {
            object: object.as_bytes().to_vec(),
            size: *size,
            executable: *executable,
        }),
        EntryContent::Directory => Content::Directory(Empty {}),
        EntryContent::Symlink { target } => Content::Symlink(target.clone()),
        EntryContent::Deleted => Content::Deleted(Empty {}),
    };
    WireEntry {
        mount_id: mount_id_bytes(&record.key.mount),
        path: record.key.path.as_str().to_owned(),
        content: Some(content),
        vector: record
            .vector
            .iter()
            .map(|(device, value)| Counter {
                device_id: device.as_bytes().to_vec(),
                value,
            })
            .collect(),
        parent_object: record.parent_object.map(|o| o.as_bytes().to_vec()),
        modified_by: record.modified_by.as_bytes().to_vec(),
        modified_at_unix_ms: record.modified_at_unix_ms,
        sequence: record.sequence.0,
        mtime_unix_ns: record.stat.map(|s| s.mtime_ns),
    }
}

/// Validate and convert a received entry. Paths go through the same
/// [`LogicalPath`] validation as local scans, so a peer cannot smuggle `..`,
/// absolute paths or reserved names into the index.
pub fn entry_from_wire(space: SpaceId, wire: WireEntry) -> Result<RemoteEntry, ProtoError> {
    use wire_entry::Content;
    let path = LogicalPath::new(&wire.path).map_err(|e| invalid("path", e.to_string()))?;
    let content = match wire.content {
        Some(Content::File(f)) => EntryContent::File {
            object: object_id_from_bytes(&f.object)?,
            size: f.size,
            executable: f.executable,
        },
        Some(Content::Directory(_)) => EntryContent::Directory,
        Some(Content::Symlink(target)) => EntryContent::Symlink { target },
        Some(Content::Deleted(_)) => EntryContent::Deleted,
        None => return Err(invalid("content", "missing")),
    };
    let mut vector = VersionVector::new();
    for counter in wire.vector {
        vector.set(device_id_from_bytes(&counter.device_id)?, counter.value);
    }
    if vector.is_empty() {
        return Err(invalid("vector", "empty version vector"));
    }
    Ok(RemoteEntry {
        key: EntryKey {
            space,
            mount: mount_id_from_bytes(&wire.mount_id)?,
            path,
        },
        content,
        vector,
        parent_object: wire
            .parent_object
            .as_deref()
            .map(object_id_from_bytes)
            .transpose()?,
        modified_by: device_id_from_bytes(&wire.modified_by)?,
        modified_at_unix_ms: wire.modified_at_unix_ms,
        sequence: Sequence(wire.sequence),
        mtime_ns: wire.mtime_unix_ns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> EntryRecord {
        EntryRecord::local_write(
            None,
            EntryKey {
                space: SpaceId::new(),
                mount: MountId::new(),
                path: LogicalPath::new("Mods/Foo/Foo.lua").unwrap(),
            },
            EntryContent::File {
                object: ObjectId::of(b"print('hi')"),
                size: 11,
                executable: false,
            },
            None,
            DeviceId::from_bytes([7; 32]),
            1_700_000_000_000,
            Sequence(42),
        )
    }

    #[test]
    fn entry_round_trips_through_the_wire() {
        let record = sample();
        let frame = Frame::new(frame::Body::IndexBatch(IndexBatch {
            space_id: space_id_bytes(&record.key.space),
            entries: vec![entry_to_wire(&record)],
            through_sequence: 42,
            caught_up: true,
            after_sequence: 0,
            plan_files: Some(1),
            plan_bytes: Some(11),
            plan_after: Some(0),
        }));
        let bytes = encode_frame(&frame).unwrap();
        let len = frame_len(bytes[..4].try_into().unwrap()).unwrap();
        assert_eq!(len, bytes.len() - 4);
        let decoded: Frame = decode_message(&bytes[4..]).unwrap();
        let Some(frame::Body::IndexBatch(batch)) = decoded.body else {
            panic!("wrong body");
        };
        let space = space_id_from_bytes(&batch.space_id).unwrap();
        let remote = entry_from_wire(space, batch.entries[0].clone()).unwrap();
        assert_eq!(remote.key, record.key);
        assert_eq!(remote.content, record.content);
        assert_eq!(remote.vector, record.vector);
        assert_eq!(remote.modified_by, record.modified_by);
        assert_eq!(remote.sequence, record.sequence);
        assert_eq!(remote.mtime_ns, record.stat.map(|s| s.mtime_ns));
        assert_eq!(batch.plan_files, Some(1));
        assert_eq!(batch.plan_bytes, Some(11));
        assert_eq!(batch.plan_after, Some(0));
    }

    #[test]
    fn hostile_paths_are_rejected() {
        let mut wire = entry_to_wire(&sample());
        // `C:\x` is a legal macOS file name, so it is a valid identity here;
        // relay-fs refuses to turn it into a Windows path.
        for bad in ["../escape", "/etc/passwd", "a/../../b", "a//b", ""] {
            wire.path = bad.to_owned();
            assert!(
                entry_from_wire(SpaceId::new(), wire.clone()).is_err(),
                "{bad:?} accepted"
            );
        }
    }

    #[test]
    fn oversized_frames_are_rejected_before_allocation() {
        let prefix = u32::try_from(MAX_FRAME_BYTES + 1).unwrap().to_be_bytes();
        assert!(frame_len(prefix).is_err());
    }
}
