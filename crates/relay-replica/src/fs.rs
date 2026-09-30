//! Filesystem durable replica.
//!
//! Layout under the configured root:
//! ```text
//! objects/<aa>/<bb>/<full 64-char hex>          plaintext, legacy
//! objects/sealed/<space_hex>/<object hex>      space-key ciphertext
//! entries/<device_hex>/<space_hex>.log
//! acks/<reader_hex>/<author_hex>/<space_hex>
//! keys/box/<device_hex>                        X25519 public || signature
//! keys/wrap/<space_hex>/<generation>/<device>
//! keys/recovery/<space_hex>/<generation>
//! nat/<device_hex>                             one address per line
//! tmp/
//! ```
//!
//! Each log record is `u64 BE unix_ms || u32 BE prost_len || prost(WireEntry)`.
//! The timestamp is for GC grace only; the apply path uses the prost bytes.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prost::Message;
use relay_core::{DeviceId, ObjectId, SpaceId};
use relay_proto::WireEntry;
use tempfile::NamedTempFile;

use crate::error::ReplicaError;
use crate::{DurableReplica, GcReport, ReplicaMode};

const OBJECTS_DIR: &str = "objects";
const SEALED_DIR: &str = "sealed";
const ENTRIES_DIR: &str = "entries";
const ACKS_DIR: &str = "acks";
const KEYS_DIR: &str = "keys";
const NAT_DIR: &str = "nat";
const TMP_DIR: &str = "tmp";

/// A space-key wrap read from the mailbox.
#[derive(Clone, Debug)]
pub struct StoredKeyWrap {
    pub space: SpaceId,
    pub generation: u32,
    pub recipient: DeviceId,
    pub wrapped: Vec<u8>,
}

#[derive(Debug)]
pub struct FsReplica {
    root: PathBuf,
    /// Cheap unchanged-log probe for [`DurableReplica::entries_after`].
    log_sig: Mutex<HashMap<(DeviceId, SpaceId), LogSig>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LogSig {
    len: u64,
    modified: Option<Duration>,
    last_seq: u64,
}

#[derive(Clone, Debug)]
struct StoredEntry {
    written_ms: u64,
    wire: WireEntry,
}

#[derive(Clone, Debug)]
struct AckRecord {
    reader: DeviceId,
    author: DeviceId,
    space: SpaceId,
    through: u64,
    #[allow(dead_code)]
    path: PathBuf,
}

impl FsReplica {
    /// Open (or create) a replica directory at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ReplicaError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| ReplicaError::io(&root, e))?;
        let meta = fs::metadata(&root).map_err(|e| ReplicaError::io(&root, e))?;
        if !meta.is_dir() {
            return Err(ReplicaError::NotADirectory(root));
        }
        let replica = Self {
            root,
            log_sig: Mutex::new(HashMap::new()),
        };
        create_dir(&replica.objects_dir())?;
        create_dir(&replica.objects_dir().join(SEALED_DIR))?;
        create_dir(&replica.entries_dir())?;
        create_dir(&replica.acks_dir())?;
        create_dir(&replica.root.join(KEYS_DIR))?;
        create_dir(&replica.root.join(NAT_DIR))?;
        create_dir(&replica.tmp_dir())?;
        Ok(replica)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn objects_dir(&self) -> PathBuf {
        self.root.join(OBJECTS_DIR)
    }

    fn entries_dir(&self) -> PathBuf {
        self.root.join(ENTRIES_DIR)
    }

    fn acks_dir(&self) -> PathBuf {
        self.root.join(ACKS_DIR)
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join(TMP_DIR)
    }

    fn object_path(&self, id: &ObjectId) -> PathBuf {
        let hex = id.to_hex();
        self.objects_dir()
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(&hex)
    }

    fn entry_log_path(&self, device: DeviceId, space: SpaceId) -> PathBuf {
        self.entries_dir()
            .join(device.to_string())
            .join(format!("{}.log", space_hex(space)))
    }

    fn ack_path(&self, reader: DeviceId, author: DeviceId, space: SpaceId) -> PathBuf {
        self.acks_dir()
            .join(reader.to_string())
            .join(author.to_string())
            .join(space_hex(space))
    }

    fn sealed_object_path(&self, space: SpaceId, id: &ObjectId) -> PathBuf {
        self.objects_dir()
            .join(SEALED_DIR)
            .join(space_hex(space))
            .join(id.to_hex())
    }

    pub fn put_sealed_object(
        &self,
        space: SpaceId,
        id: &ObjectId,
        sealed: &[u8],
    ) -> Result<(), ReplicaError> {
        let dest = self.sealed_object_path(space, id);
        if dest.is_file() {
            return Ok(());
        }
        if let Some(parent) = dest.parent() {
            create_dir(parent)?;
        }
        atomic_write(&dest, sealed, &self.tmp_dir())
    }

    pub fn get_sealed_object(
        &self,
        space: SpaceId,
        id: &ObjectId,
    ) -> Result<Option<Vec<u8>>, ReplicaError> {
        let path = self.sealed_object_path(space, id);
        if !path.is_file() {
            return Ok(None);
        }
        fs::read(&path)
            .map(Some)
            .map_err(|e| ReplicaError::io(&path, e))
    }

    pub fn remove_plaintext_object(&self, id: &ObjectId) -> Result<(), ReplicaError> {
        let path = self.object_path(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ReplicaError::io(&path, e)),
        }
    }

    pub fn put_box_key(&self, device: DeviceId, body: &[u8]) -> Result<(), ReplicaError> {
        let dest = self
            .root
            .join(KEYS_DIR)
            .join("box")
            .join(device.to_string());
        write_if_changed(&dest, body, &self.tmp_dir())
    }

    pub fn list_box_keys(&self) -> Result<Vec<(DeviceId, Vec<u8>)>, ReplicaError> {
        let dir = self.root.join(KEYS_DIR).join("box");
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| ReplicaError::io(&dir, e))? {
            let entry = entry.map_err(|e| ReplicaError::io(&dir, e))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Ok(device) = name.parse::<DeviceId>() else {
                continue;
            };
            let bytes = fs::read(entry.path()).map_err(|e| ReplicaError::io(entry.path(), e))?;
            out.push((device, bytes));
        }
        Ok(out)
    }

    pub fn put_key_wrap(
        &self,
        space: SpaceId,
        generation: u32,
        recipient: DeviceId,
        wrapped: &[u8],
    ) -> Result<(), ReplicaError> {
        let dest = self
            .root
            .join(KEYS_DIR)
            .join("wrap")
            .join(space_hex(space))
            .join(generation.to_string())
            .join(recipient.to_string());
        write_if_changed(&dest, wrapped, &self.tmp_dir())
    }

    pub fn list_key_wraps(&self) -> Result<Vec<StoredKeyWrap>, ReplicaError> {
        let root = self.root.join(KEYS_DIR).join("wrap");
        if !root.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for space_ent in fs::read_dir(&root).map_err(|e| ReplicaError::io(&root, e))? {
            let space_ent = space_ent.map_err(|e| ReplicaError::io(&root, e))?;
            let Some(space_name) = space_ent.file_name().to_str().map(|s| s.to_owned()) else {
                continue;
            };
            let Ok(space) = parse_space_hex(&space_name) else {
                continue;
            };
            let space_dir = space_ent.path();
            if !space_dir.is_dir() {
                continue;
            }
            for gen_ent in fs::read_dir(&space_dir).map_err(|e| ReplicaError::io(&space_dir, e))? {
                let gen_ent = gen_ent.map_err(|e| ReplicaError::io(&space_dir, e))?;
                let Some(gen_name) = gen_ent.file_name().to_str().map(|s| s.to_owned()) else {
                    continue;
                };
                let Ok(generation) = gen_name.parse::<u32>() else {
                    continue;
                };
                let gen_dir = gen_ent.path();
                if !gen_dir.is_dir() {
                    continue;
                }
                for rec_ent in fs::read_dir(&gen_dir).map_err(|e| ReplicaError::io(&gen_dir, e))? {
                    let rec_ent = rec_ent.map_err(|e| ReplicaError::io(&gen_dir, e))?;
                    let Some(rec_name) = rec_ent.file_name().to_str().map(|s| s.to_owned()) else {
                        continue;
                    };
                    let Ok(recipient) = rec_name.parse::<DeviceId>() else {
                        continue;
                    };
                    let bytes = fs::read(rec_ent.path())
                        .map_err(|e| ReplicaError::io(rec_ent.path(), e))?;
                    out.push(StoredKeyWrap {
                        space,
                        generation,
                        recipient,
                        wrapped: bytes,
                    });
                }
            }
        }
        Ok(out)
    }

    pub fn put_recovery_wrap(
        &self,
        space: SpaceId,
        generation: u32,
        wrapped: &[u8],
    ) -> Result<(), ReplicaError> {
        let dest = self
            .root
            .join(KEYS_DIR)
            .join("recovery")
            .join(space_hex(space))
            .join(generation.to_string());
        write_if_changed(&dest, wrapped, &self.tmp_dir())
    }

    pub fn list_recovery_wraps(&self) -> Result<Vec<(SpaceId, u32, Vec<u8>)>, ReplicaError> {
        let root = self.root.join(KEYS_DIR).join("recovery");
        if !root.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for space_ent in fs::read_dir(&root).map_err(|e| ReplicaError::io(&root, e))? {
            let space_ent = space_ent.map_err(|e| ReplicaError::io(&root, e))?;
            let Some(space_name) = space_ent.file_name().to_str().map(|s| s.to_owned()) else {
                continue;
            };
            let Ok(space) = parse_space_hex(&space_name) else {
                continue;
            };
            let space_dir = space_ent.path();
            if !space_dir.is_dir() {
                continue;
            }
            for gen_ent in fs::read_dir(&space_dir).map_err(|e| ReplicaError::io(&space_dir, e))? {
                let gen_ent = gen_ent.map_err(|e| ReplicaError::io(&space_dir, e))?;
                if !gen_ent.path().is_file() {
                    continue;
                }
                let Some(gen_name) = gen_ent.file_name().to_str().map(|s| s.to_owned()) else {
                    continue;
                };
                let Ok(generation) = gen_name.parse::<u32>() else {
                    continue;
                };
                let bytes =
                    fs::read(gen_ent.path()).map_err(|e| ReplicaError::io(gen_ent.path(), e))?;
                out.push((space, generation, bytes));
            }
        }
        Ok(out)
    }

    pub fn put_nat_candidates(
        &self,
        device: DeviceId,
        addrs: &[String],
    ) -> Result<(), ReplicaError> {
        let dest = self.root.join(NAT_DIR).join(device.to_string());
        let mut body = String::new();
        for addr in addrs.iter().take(8) {
            if addr.len() > 200 || addr.contains('\n') {
                continue;
            }
            body.push_str(addr);
            body.push('\n');
        }
        write_if_changed(&dest, body.as_bytes(), &self.tmp_dir())
    }

    pub fn list_nat_candidates(&self) -> Result<Vec<(DeviceId, Vec<String>)>, ReplicaError> {
        let dir = self.root.join(NAT_DIR);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| ReplicaError::io(&dir, e))? {
            let entry = entry.map_err(|e| ReplicaError::io(&dir, e))?;
            let Some(name) = entry.file_name().to_str().map(|s| s.to_owned()) else {
                continue;
            };
            let Ok(device) = name.parse::<DeviceId>() else {
                continue;
            };
            let text =
                fs::read_to_string(entry.path()).map_err(|e| ReplicaError::io(entry.path(), e))?;
            let addrs = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .take(8)
                .map(str::to_owned)
                .collect();
            out.push((device, addrs));
        }
        Ok(out)
    }

    fn read_log(&self, path: &Path) -> Result<Vec<StoredEntry>, ReplicaError> {
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let data = fs::read(path).map_err(|e| ReplicaError::io(path, e))?;
        decode_log(&data)
    }

    fn write_log_atomic(&self, path: &Path, entries: &[StoredEntry]) -> Result<(), ReplicaError> {
        if let Some(parent) = path.parent() {
            create_dir(parent)?;
        }
        let mut body = Vec::new();
        for entry in entries {
            encode_record(&mut body, entry.written_ms, &entry.wire)?;
        }
        atomic_write(path, &body, &self.tmp_dir())
    }

    fn list_entry_logs(&self) -> Result<Vec<(DeviceId, SpaceId, PathBuf)>, ReplicaError> {
        let root = self.entries_dir();
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for device_ent in fs::read_dir(&root).map_err(|e| ReplicaError::io(&root, e))? {
            let device_ent = device_ent.map_err(|e| ReplicaError::io(&root, e))?;
            if !device_ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(device_name) = device_ent.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(device) = device_name.parse::<DeviceId>() else {
                continue;
            };
            let device_dir = device_ent.path();
            for log_ent in
                fs::read_dir(&device_dir).map_err(|e| ReplicaError::io(&device_dir, e))?
            {
                let log_ent = log_ent.map_err(|e| ReplicaError::io(&device_dir, e))?;
                if !log_ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let name = log_ent.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let Some(space_hex) = name.strip_suffix(".log") else {
                    continue;
                };
                let Ok(space) = parse_space_hex(space_hex) else {
                    continue;
                };
                out.push((device, space, log_ent.path()));
            }
        }
        Ok(out)
    }

    fn list_acks(&self) -> Result<Vec<AckRecord>, ReplicaError> {
        let root = self.acks_dir();
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for reader_ent in fs::read_dir(&root).map_err(|e| ReplicaError::io(&root, e))? {
            let reader_ent = reader_ent.map_err(|e| ReplicaError::io(&root, e))?;
            if !reader_ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(reader_name) = reader_ent.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(reader) = reader_name.parse::<DeviceId>() else {
                continue;
            };
            let reader_dir = reader_ent.path();
            for author_ent in
                fs::read_dir(&reader_dir).map_err(|e| ReplicaError::io(&reader_dir, e))?
            {
                let author_ent = author_ent.map_err(|e| ReplicaError::io(&reader_dir, e))?;
                if !author_ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let Some(author_name) = author_ent.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Ok(author) = author_name.parse::<DeviceId>() else {
                    continue;
                };
                let author_dir = author_ent.path();
                for space_ent in
                    fs::read_dir(&author_dir).map_err(|e| ReplicaError::io(&author_dir, e))?
                {
                    let space_ent = space_ent.map_err(|e| ReplicaError::io(&author_dir, e))?;
                    if !space_ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                        continue;
                    }
                    let Some(space_name) = space_ent.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    let Ok(space) = parse_space_hex(&space_name) else {
                        continue;
                    };
                    let path = space_ent.path();
                    let through = read_ack_file(&path)?;
                    out.push(AckRecord {
                        reader,
                        author,
                        space,
                        through,
                        path,
                    });
                }
            }
        }
        Ok(out)
    }

    fn list_object_ids(&self) -> Result<Vec<(ObjectId, PathBuf, u64)>, ReplicaError> {
        let root = self.objects_dir();
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in walkdir_files(&root)? {
            let Some(name) = entry.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Ok(id) = name.parse::<ObjectId>() else {
                continue;
            };
            let meta = fs::metadata(&entry).map_err(|e| ReplicaError::io(&entry, e))?;
            out.push((id, entry, meta.len()));
        }
        Ok(out)
    }
}

impl DurableReplica for FsReplica {
    fn put_object(&mut self, id: ObjectId, bytes: &[u8]) -> Result<(), ReplicaError> {
        let actual = ObjectId::of(bytes);
        if actual != id {
            return Err(ReplicaError::ObjectIdMismatch {
                expected: id,
                actual,
            });
        }
        let dest = self.object_path(&id);
        if dest.is_file() {
            return Ok(());
        }
        if let Some(parent) = dest.parent() {
            create_dir(parent)?;
        }
        atomic_write(&dest, bytes, &self.tmp_dir())
    }

    fn get_object(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, ReplicaError> {
        let path = self.object_path(id);
        if !path.is_file() {
            return Ok(None);
        }
        let data = fs::read(&path).map_err(|e| ReplicaError::io(&path, e))?;
        let actual = ObjectId::of(&data);
        if actual != *id {
            return Err(ReplicaError::CorruptObject { id: *id, actual });
        }
        Ok(Some(data))
    }

    fn append_entries(
        &mut self,
        device: DeviceId,
        space: SpaceId,
        entries: &[WireEntry],
    ) -> Result<(), ReplicaError> {
        if entries.is_empty() {
            return Ok(());
        }
        let path = self.entry_log_path(device, space);
        let mut stored = self.read_log(&path)?;
        let mut last = stored.last().map(|e| e.wire.sequence);
        let now_ms = unix_now_ms();
        let mut changed = false;

        for wire in entries {
            if let Some(existing) = stored.iter().find(|e| e.wire.sequence == wire.sequence) {
                if !wire_eq(&existing.wire, wire) {
                    return Err(ReplicaError::SequenceConflict {
                        sequence: wire.sequence,
                    });
                }
                continue;
            }
            if let Some(prev) = last
                && wire.sequence <= prev
            {
                return Err(ReplicaError::SequenceGap {
                    expected: prev + 1,
                    got: wire.sequence,
                });
            }
            stored.push(StoredEntry {
                written_ms: now_ms,
                wire: wire.clone(),
            });
            last = Some(wire.sequence);
            changed = true;
        }
        if changed {
            self.write_log_atomic(&path, &stored)?;
            if let Ok(mut cache) = self.log_sig.lock() {
                cache.remove(&(device, space));
            }
        }
        Ok(())
    }

    fn entries_after(
        &self,
        device: DeviceId,
        space: SpaceId,
        after_sequence: u64,
    ) -> Result<Vec<WireEntry>, ReplicaError> {
        let path = self.entry_log_path(device, space);
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let meta = fs::metadata(&path).map_err(|e| ReplicaError::io(&path, e))?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok());
        let sig_key = (device, space);
        if let Ok(cache) = self.log_sig.lock()
            && let Some(prev) = cache.get(&sig_key)
            && prev.len == meta.len()
            && prev.modified == modified
            && after_sequence >= prev.last_seq
        {
            return Ok(Vec::new());
        }
        let stored = self.read_log(&path)?;
        let last_seq = stored.last().map(|e| e.wire.sequence).unwrap_or(0);
        if let Ok(mut cache) = self.log_sig.lock() {
            cache.insert(
                sig_key,
                LogSig {
                    len: meta.len(),
                    modified,
                    last_seq,
                },
            );
        }
        Ok(stored
            .into_iter()
            .filter(|e| e.wire.sequence > after_sequence)
            .map(|e| e.wire)
            .collect())
    }

    fn put_ack(
        &mut self,
        reader: DeviceId,
        author: DeviceId,
        space: SpaceId,
        through_sequence: u64,
    ) -> Result<(), ReplicaError> {
        let path = self.ack_path(reader, author, space);
        if let Some(parent) = path.parent() {
            create_dir(parent)?;
        }
        let current = if path.is_file() {
            read_ack_file(&path)?
        } else {
            0
        };
        if through_sequence < current {
            return Ok(());
        }
        atomic_write(
            &path,
            through_sequence.to_string().as_bytes(),
            &self.tmp_dir(),
        )
    }

    fn ack(&self, reader: DeviceId, author: DeviceId, space: SpaceId) -> Result<u64, ReplicaError> {
        let path = self.ack_path(reader, author, space);
        if !path.is_file() {
            return Ok(0);
        }
        read_ack_file(&path)
    }

    fn gc(
        &mut self,
        mode: ReplicaMode,
        grace: Duration,
        members: &[(SpaceId, DeviceId)],
    ) -> Result<GcReport, ReplicaError> {
        if let Ok(mut cache) = self.log_sig.lock() {
            cache.clear();
        }
        let now_ms = unix_now_ms();
        let grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX);
        let acks = self.list_acks()?;
        if acks.is_empty() {
            return Ok(GcReport::default());
        }

        let logs = self.list_entry_logs()?;
        let mut report = GcReport::default();
        // Objects still referenced by retained log entries.
        let mut live_objects: HashSet<ObjectId> = HashSet::new();
        // Mirror: objects of the latest live entry per (mount, path) in each log.
        let mut mirror_keep: HashSet<ObjectId> = HashSet::new();

        for (device, space, path) in &logs {
            let mut stored = self.read_log(path)?;
            if mode == ReplicaMode::Mirror {
                let mut latest_live: HashMap<(Vec<u8>, String), ObjectId> = HashMap::new();
                for entry in &stored {
                    if let Some(obj) = wire_object(&entry.wire) {
                        // Tombstone clears the tip for this path.
                        if matches!(
                            entry.wire.content,
                            Some(relay_proto::wire_entry::Content::Deleted(_))
                        ) {
                            latest_live
                                .remove(&(entry.wire.mount_id.clone(), entry.wire.path.clone()));
                        } else {
                            latest_live.insert(
                                (entry.wire.mount_id.clone(), entry.wire.path.clone()),
                                obj,
                            );
                        }
                    } else if matches!(
                        entry.wire.content,
                        Some(relay_proto::wire_entry::Content::Deleted(_))
                    ) {
                        latest_live.remove(&(entry.wire.mount_id.clone(), entry.wire.path.clone()));
                    }
                }
                mirror_keep.extend(latest_live.values().copied());
            }

            let readers: Vec<DeviceId> = members
                .iter()
                .filter(|(s, id)| *s == *space && *id != *device)
                .map(|(_, id)| *id)
                .collect();
            let before = stored.len();
            stored.retain(|entry| {
                let acked = !readers.is_empty()
                    && readers.iter().all(|reader| {
                        acks.iter().any(|ack| {
                            ack.reader == *reader
                                && ack.author == *device
                                && ack.space == *space
                                && ack.through >= entry.wire.sequence
                        })
                    });
                let old_enough = now_ms.saturating_sub(entry.written_ms) >= grace_ms;
                // Keep if any member has not acked, or the entry is still within grace.
                !(acked && old_enough)
            });
            report.entries_removed += before - stored.len();
            for entry in &stored {
                if let Some(obj) = wire_object(&entry.wire) {
                    live_objects.insert(obj);
                }
            }
            if stored.len() != before {
                self.write_log_atomic(path, &stored)?;
            }
        }

        // Recompute live set after all log rewrites (mirror keep applies).
        live_objects.clear();
        for (_, _, path) in &logs {
            for entry in self.read_log(path)? {
                if let Some(obj) = wire_object(&entry.wire) {
                    live_objects.insert(obj);
                }
            }
        }
        if mode == ReplicaMode::Mirror {
            live_objects.extend(mirror_keep.iter().copied());
        }

        for (id, path, len) in self.list_object_ids()? {
            if live_objects.contains(&id) {
                continue;
            }
            // Only delete objects that were candidates under the ack rule:
            // if no remaining log references them, they were only referenced by
            // removed entries (or were never in a log). Never delete while any
            // remaining entry references them (live_objects). Objects with no
            // ack coverage for their author must not be deleted by mailbox —
            // those stay only if still in a log. Orphans with no log ref and
            // at least one ack somewhere are removable.
            fs::remove_file(&path).map_err(|e| ReplicaError::io(&path, e))?;
            report.objects_removed += 1;
            report.bytes_freed = report.bytes_freed.saturating_add(len);
        }

        Ok(report)
    }
}

fn wire_object(wire: &WireEntry) -> Option<ObjectId> {
    match &wire.content {
        Some(relay_proto::wire_entry::Content::File(f)) => {
            let arr: [u8; 32] = f.object.as_slice().try_into().ok()?;
            Some(ObjectId::from_bytes(arr))
        }
        _ => None,
    }
}

fn wire_eq(a: &WireEntry, b: &WireEntry) -> bool {
    a.encode_to_vec() == b.encode_to_vec()
}

fn encode_record(out: &mut Vec<u8>, written_ms: u64, wire: &WireEntry) -> Result<(), ReplicaError> {
    let mut body = Vec::new();
    wire.encode(&mut body)
        .map_err(|e| ReplicaError::CorruptLog(e.to_string()))?;
    out.extend_from_slice(&written_ms.to_be_bytes());
    let len = u32::try_from(body.len())
        .map_err(|_| ReplicaError::CorruptLog(format!("entry too large: {} bytes", body.len())))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(())
}

fn decode_log(data: &[u8]) -> Result<Vec<StoredEntry>, ReplicaError> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if data.len() - i < 12 {
            return Err(ReplicaError::CorruptLog(
                "truncated timestamp/length prefix".into(),
            ));
        }
        let written_ms = u64::from_be_bytes(data[i..i + 8].try_into().unwrap());
        i += 8;
        let len = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        if data.len() - i < len {
            return Err(ReplicaError::CorruptLog("truncated entry body".into()));
        }
        let wire = WireEntry::decode(&data[i..i + len])
            .map_err(|e| ReplicaError::CorruptLog(e.to_string()))?;
        i += len;
        out.push(StoredEntry { written_ms, wire });
    }
    Ok(out)
}

fn read_ack_file(path: &Path) -> Result<u64, ReplicaError> {
    let raw = fs::read_to_string(path).map_err(|e| ReplicaError::io(path, e))?;
    let trimmed = raw.trim();
    trimmed
        .parse::<u64>()
        .map_err(|_| ReplicaError::CorruptAck(format!("not a u64: {trimmed:?}")))
}

fn space_hex(space: SpaceId) -> String {
    hex::encode(space.as_uuid().as_bytes())
}

fn parse_space_hex(s: &str) -> Result<SpaceId, ()> {
    let mut bytes = [0u8; 16];
    hex::decode_to_slice(s, &mut bytes).map_err(|_| ())?;
    Ok(SpaceId::from_uuid(uuid::Uuid::from_bytes(bytes)))
}

fn create_dir(path: &Path) -> Result<(), ReplicaError> {
    fs::create_dir_all(path).map_err(|e| ReplicaError::io(path, e))
}

fn write_if_changed(dest: &Path, bytes: &[u8], tmp_dir: &Path) -> Result<(), ReplicaError> {
    if fs::read(dest).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        create_dir(parent)?;
    }
    atomic_write(dest, bytes, tmp_dir)
}

fn atomic_write(dest: &Path, bytes: &[u8], tmp_dir: &Path) -> Result<(), ReplicaError> {
    create_dir(tmp_dir)?;
    let mut tmp = NamedTempFile::new_in(tmp_dir).map_err(|e| ReplicaError::io(tmp_dir, e))?;
    tmp.write_all(bytes)
        .map_err(|e| ReplicaError::io(tmp.path(), e))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| ReplicaError::io(tmp.path(), e))?;
    let tmp_path = tmp.into_temp_path();
    tmp_path
        .persist(dest)
        .map_err(|e| ReplicaError::io(dest, e.error))?;
    // Best-effort durability of the directory entry.
    if let Some(parent) = dest.parent()
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn walkdir_files(root: &Path) -> Result<Vec<PathBuf>, ReplicaError> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), ReplicaError> {
        for entry in fs::read_dir(dir).map_err(|e| ReplicaError::io(dir, e))? {
            let entry = entry.map_err(|e| ReplicaError::io(dir, e))?;
            let path = entry.path();
            let ft = entry.file_type().map_err(|e| ReplicaError::io(&path, e))?;
            if ft.is_dir() {
                walk(&path, out)?;
            } else if ft.is_file() {
                out.push(path);
            }
        }
        Ok(())
    }
    walk(root, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_core::{DeviceId, ObjectId, SpaceId};
    use relay_proto::{Empty, WireFile, wire_entry};

    fn device(n: u8) -> DeviceId {
        DeviceId::from_bytes([n; 32])
    }

    fn file_entry(seq: u64, path: &str, bytes: &[u8]) -> (WireEntry, ObjectId) {
        let id = ObjectId::of(bytes);
        let wire = WireEntry {
            mount_id: vec![1; 16],
            path: path.into(),
            content: Some(wire_entry::Content::File(WireFile {
                object: id.as_bytes().to_vec(),
                size: bytes.len() as u64,
                executable: false,
            })),
            vector: vec![],
            parent_object: None,
            modified_by: device(1).as_bytes().to_vec(),
            modified_at_unix_ms: 1,
            sequence: seq,
            mtime_unix_ns: None,
        };
        (wire, id)
    }

    fn deleted_entry(seq: u64, path: &str) -> WireEntry {
        WireEntry {
            mount_id: vec![1; 16],
            path: path.into(),
            content: Some(wire_entry::Content::Deleted(Empty {})),
            vector: vec![],
            parent_object: None,
            modified_by: device(1).as_bytes().to_vec(),
            modified_at_unix_ms: 1,
            sequence: seq,
            mtime_unix_ns: None,
        }
    }

    #[test]
    fn put_get_object_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut r = FsReplica::open(dir.path()).unwrap();
        let bytes = b"hello-mailbox";
        let id = ObjectId::of(bytes);
        r.put_object(id, bytes).unwrap();
        assert_eq!(
            r.get_object(&id).unwrap().as_deref(),
            Some(bytes.as_slice())
        );
        assert!(r.get_object(&ObjectId::of(b"missing")).unwrap().is_none());
    }

    #[test]
    fn put_object_rejects_id_mismatch() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut r = FsReplica::open(dir.path()).unwrap();
        let err = r.put_object(ObjectId::of(b"a"), b"b").unwrap_err();
        assert!(matches!(err, ReplicaError::ObjectIdMismatch { .. }));
    }

    #[test]
    fn append_is_idempotent_and_ordered() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut r = FsReplica::open(dir.path()).unwrap();
        let author = device(1);
        let space = SpaceId::new();
        let (e1, _) = file_entry(1, "a.txt", b"a");
        let (e2, _) = file_entry(2, "b.txt", b"bb");
        r.append_entries(author, space, &[e1.clone(), e2.clone()])
            .unwrap();
        r.append_entries(author, space, &[e1.clone(), e2.clone()])
            .unwrap();
        let got = r.entries_after(author, space, 0).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].sequence, 1);
        assert_eq!(got[1].sequence, 2);

        let (conflict, _) = file_entry(1, "a.txt", b"other");
        let err = r.append_entries(author, space, &[conflict]).unwrap_err();
        assert!(matches!(
            err,
            ReplicaError::SequenceConflict { sequence: 1 }
        ));
    }

    #[test]
    fn gc_mailbox_requires_acks_and_grace() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut r = FsReplica::open(dir.path()).unwrap();
        let author = device(1);
        let reader = device(2);
        let space = SpaceId::new();
        let (e1, id1) = file_entry(1, "a.txt", b"v1");
        let (e2, id2) = file_entry(2, "a.txt", b"v2-longer");
        r.put_object(id1, b"v1").unwrap();
        r.put_object(id2, b"v2-longer").unwrap();
        r.append_entries(author, space, &[e1, e2]).unwrap();

        // No acks → nothing deleted.
        let members = [(space, author), (space, reader)];
        let report = r
            .gc(ReplicaMode::Mailbox, Duration::ZERO, &members)
            .unwrap();
        assert_eq!(report.entries_removed, 0);
        assert_eq!(report.objects_removed, 0);
        assert!(r.get_object(&id1).unwrap().is_some());

        r.put_ack(reader, author, space, 2).unwrap();
        // Backdate written_ms by rewriting the log with old timestamps.
        {
            let path = r.entry_log_path(author, space);
            let mut stored = r.read_log(&path).unwrap();
            for e in &mut stored {
                e.written_ms = 1;
            }
            r.write_log_atomic(&path, &stored).unwrap();
        }

        let quiet = device(3);
        let held = [(space, author), (space, reader), (space, quiet)];
        let report = r
            .gc(ReplicaMode::Mailbox, Duration::from_secs(1), &held)
            .unwrap();
        assert_eq!(report.entries_removed, 0, "a member with no ack blocks GC");

        let report = r
            .gc(ReplicaMode::Mailbox, Duration::from_secs(1), &members)
            .unwrap();
        assert_eq!(report.entries_removed, 2);
        assert_eq!(report.objects_removed, 2);
        assert!(r.get_object(&id1).unwrap().is_none());
        assert!(r.get_object(&id2).unwrap().is_none());
        assert!(r.entries_after(author, space, 0).unwrap().is_empty());
    }

    #[test]
    fn gc_mirror_keeps_latest_live_object() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut r = FsReplica::open(dir.path()).unwrap();
        let author = device(1);
        let reader = device(2);
        let space = SpaceId::new();
        let (e1, id1) = file_entry(1, "a.txt", b"v1");
        let (e2, id2) = file_entry(2, "a.txt", b"v2-longer");
        r.put_object(id1, b"v1").unwrap();
        r.put_object(id2, b"v2-longer").unwrap();
        r.append_entries(author, space, &[e1, e2]).unwrap();
        r.put_ack(reader, author, space, 2).unwrap();
        {
            let path = r.entry_log_path(author, space);
            let mut stored = r.read_log(&path).unwrap();
            for e in &mut stored {
                e.written_ms = 1;
            }
            r.write_log_atomic(&path, &stored).unwrap();
        }

        let members = [(space, author), (space, reader)];
        let report = r
            .gc(ReplicaMode::Mirror, Duration::from_secs(1), &members)
            .unwrap();
        assert_eq!(report.entries_removed, 2);
        assert_eq!(report.objects_removed, 1);
        assert!(r.get_object(&id1).unwrap().is_none());
        assert!(r.get_object(&id2).unwrap().is_some());
    }

    #[test]
    fn gc_mirror_tombstone_allows_object_removal() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut r = FsReplica::open(dir.path()).unwrap();
        let author = device(1);
        let reader = device(2);
        let space = SpaceId::new();
        let (e1, id1) = file_entry(1, "a.txt", b"v1");
        let e2 = deleted_entry(2, "a.txt");
        r.put_object(id1, b"v1").unwrap();
        r.append_entries(author, space, &[e1, e2]).unwrap();
        r.put_ack(reader, author, space, 2).unwrap();
        {
            let path = r.entry_log_path(author, space);
            let mut stored = r.read_log(&path).unwrap();
            for e in &mut stored {
                e.written_ms = 1;
            }
            r.write_log_atomic(&path, &stored).unwrap();
        }
        let members = [(space, author), (space, reader)];
        let report = r
            .gc(ReplicaMode::Mirror, Duration::from_secs(1), &members)
            .unwrap();
        assert_eq!(report.entries_removed, 2);
        assert_eq!(report.objects_removed, 1);
        assert!(r.get_object(&id1).unwrap().is_none());
    }
}
