use std::collections::HashSet;
use std::path::{Path, PathBuf};

use relay_core::{
    Device, DeviceId, EntryContent, EntryKey, EntryRecord, Mount, MountId, ObjectId, Sequence,
    Space, SpaceId, StatHint, VersionVector,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::DbError;
use crate::convert::{
    decode_content, decode_stat, device_id_bytes, encode_content, encode_stat, i64_from_u64,
    is_unique_violation, map_write_err, mount_bytes, mount_from_bytes, object_id_from_blob,
    opt_object_id, space_bytes, space_from_bytes, u64_from_i64,
};

const ENTRY_SELECT: &str = "e.id, e.mount_id, e.path, e.kind, e.deleted, e.object_id, e.size,
     e.executable, e.symlink_target, e.parent_object, e.sequence,
     d.device_id, e.modified_at_ms, e.stat_size, e.stat_mtime_ns, e.stat_file_id,
     m.space_id";

#[derive(Clone, Copy)]
pub struct Repo<'c> {
    pub(crate) conn: &'c Connection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalDevice {
    pub device: Device,
    pub next_sequence: Sequence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountConfig {
    pub mount: Mount,
    pub local_path: Option<PathBuf>,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRecord {
    pub sequence: Sequence,
    pub content: EntryContent,
    pub vector: VersionVector,
    pub parent_object: Option<ObjectId>,
    pub modified_by: DeviceId,
    pub modified_at_unix_ms: i64,
}

struct RawEntry {
    id: i64,
    mount_id: [u8; 16],
    path: String,
    kind: Option<String>,
    deleted: i64,
    object_id: Option<Vec<u8>>,
    size: Option<i64>,
    executable: i64,
    symlink_target: Option<String>,
    parent_object: Option<Vec<u8>>,
    sequence: i64,
    modified_by: [u8; 32],
    modified_at_ms: i64,
    stat_size: Option<i64>,
    stat_mtime_ns: Option<i64>,
    stat_file_id: Option<i64>,
    space_id: [u8; 16],
}

impl Repo<'_> {
    pub fn init_local_device(&self, device: &Device, now_ms: i64) -> Result<(), DbError> {
        if self.local_device()?.is_some() {
            return Err(DbError::AlreadyInitialized);
        }
        self.upsert_device(device, now_ms)?;
        let device_ref = self
            .device_ref(device.id)?
            .ok_or_else(|| DbError::Corrupt("upserted local device is missing".into()))?;
        match self.conn.execute(
            "INSERT INTO local_device (singleton, device_ref, next_sequence) VALUES (1, ?1, 1)",
            params![device_ref],
        ) {
            Ok(_) => Ok(()),
            Err(err) if is_unique_violation(&err) => Err(DbError::AlreadyInitialized),
            Err(err) => Err(err.into()),
        }
    }

    pub fn local_device(&self) -> Result<Option<LocalDevice>, DbError> {
        let row = self
            .conn
            .query_row(
                "SELECT d.device_id, d.name, ld.next_sequence
                 FROM local_device ld
                 JOIN devices d ON d.ref = ld.device_ref
                 WHERE ld.singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, [u8; 32]>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((id, name, next_sequence)) => Ok(Some(LocalDevice {
                device: Device {
                    id: DeviceId::from_bytes(id),
                    name,
                },
                next_sequence: Sequence(u64_from_i64(next_sequence)?),
            })),
        }
    }

    pub fn upsert_device(&self, device: &Device, now_ms: i64) -> Result<(), DbError> {
        let id = device_id_bytes(device.id);
        self.conn.execute(
            "INSERT INTO devices (device_id, name, status, created_at_ms, last_seen_ms)
             VALUES (?1, ?2, 'active', ?3, ?3)
             ON CONFLICT(device_id) DO UPDATE SET
                name = excluded.name,
                last_seen_ms = excluded.last_seen_ms",
            params![id.as_slice(), device.name.as_str(), now_ms],
        )?;
        Ok(())
    }

    pub fn list_devices(&self) -> Result<Vec<Device>, DbError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT device_id, name FROM devices ORDER BY ref")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, [u8; 32]>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut devices = Vec::new();
        for row in rows {
            let (id, name) = row?;
            devices.push(Device {
                id: DeviceId::from_bytes(id),
                name,
            });
        }
        Ok(devices)
    }

    /// Allocate the next local sequence number (monotonic, never reused, persisted).
    pub fn next_sequence(&self) -> Result<Sequence, DbError> {
        let current: i64 = self
            .conn
            .query_row(
                "SELECT next_sequence FROM local_device WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(DbError::NotInitialized)?;
        let seq = u64_from_i64(current)?;
        let next = seq.checked_add(1).ok_or(DbError::IntegerOverflow)?;
        self.conn.execute(
            "UPDATE local_device SET next_sequence = ?1 WHERE singleton = 1",
            params![i64_from_u64(next)?],
        )?;
        Ok(Sequence(seq))
    }

    pub fn create_space(&self, space: &Space, now_ms: i64) -> Result<(), DbError> {
        let id = space_bytes(space.id);
        match self.conn.execute(
            "INSERT INTO spaces (id, name, created_at_ms) VALUES (?1, ?2, ?3)",
            params![id.as_slice(), space.name.as_str(), now_ms],
        ) {
            Ok(_) => Ok(()),
            Err(err) => Err(map_write_err(err, Some(&space.name))),
        }
    }

    pub fn space(&self, id: SpaceId) -> Result<Option<Space>, DbError> {
        let bytes = space_bytes(id);
        self.conn
            .query_row(
                "SELECT id, name FROM spaces WHERE id = ?1",
                params![bytes.as_slice()],
                |row| Ok((row.get::<_, [u8; 16]>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(id, name)| {
                Ok(Space {
                    id: space_from_bytes(id),
                    name,
                })
            })
            .transpose()
    }

    pub fn space_by_name(&self, name: &str) -> Result<Option<Space>, DbError> {
        self.conn
            .query_row(
                "SELECT id, name FROM spaces WHERE name = ?1",
                params![name],
                |row| Ok((row.get::<_, [u8; 16]>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(id, name)| {
                Ok(Space {
                    id: space_from_bytes(id),
                    name,
                })
            })
            .transpose()
    }

    pub fn list_spaces(&self) -> Result<Vec<Space>, DbError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, name FROM spaces ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, [u8; 16]>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut spaces = Vec::new();
        for row in rows {
            let (id, name) = row?;
            spaces.push(Space {
                id: space_from_bytes(id),
                name,
            });
        }
        Ok(spaces)
    }

    pub fn create_mount(&self, mount: &Mount, now_ms: i64) -> Result<(), DbError> {
        let id = mount_bytes(mount.id);
        let space = space_bytes(mount.space);
        match self.conn.execute(
            "INSERT INTO mounts (id, space_id, name, created_at_ms) VALUES (?1, ?2, ?3, ?4)",
            params![id.as_slice(), space.as_slice(), mount.name.as_str(), now_ms],
        ) {
            Ok(_) => Ok(()),
            Err(err) => Err(map_write_err(err, Some(&mount.name))),
        }
    }

    pub fn mount_by_name(&self, space: SpaceId, name: &str) -> Result<Option<Mount>, DbError> {
        let space = space_bytes(space);
        self.conn
            .query_row(
                "SELECT id, space_id, name FROM mounts WHERE space_id = ?1 AND name = ?2",
                params![space.as_slice(), name],
                |row| {
                    Ok((
                        row.get::<_, [u8; 16]>(0)?,
                        row.get::<_, [u8; 16]>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .map(|(id, space, name)| {
                Ok(Mount {
                    id: mount_from_bytes(id),
                    space: space_from_bytes(space),
                    name,
                })
            })
            .transpose()
    }

    pub fn set_local_mount_path(&self, mount: MountId, path: &Path) -> Result<(), DbError> {
        let local = self.local_device()?.ok_or(DbError::NotInitialized)?;
        let device_ref = self
            .device_ref(local.device.id)?
            .ok_or_else(|| DbError::Corrupt("local device row is missing".into()))?;
        let path = path.to_str().ok_or(DbError::NonUtf8Path)?;
        let mount = mount_bytes(mount);
        match self.conn.execute(
            "INSERT INTO device_mounts (device_ref, mount_id, local_path, mode)
             VALUES (?1, ?2, ?3, 'materialized')
             ON CONFLICT(device_ref, mount_id) DO UPDATE SET local_path = excluded.local_path",
            params![device_ref, mount.as_slice(), path],
        ) {
            Ok(_) => Ok(()),
            Err(err) => Err(map_write_err(err, None)),
        }
    }

    pub fn set_mount_rules(
        &self,
        mount: MountId,
        includes: &[String],
        excludes: &[String],
    ) -> Result<(), DbError> {
        if self.mount_space(mount)?.is_none() {
            return Err(DbError::NotFound);
        }
        let mount_blob = mount_bytes(mount);
        self.conn.execute(
            "DELETE FROM mount_rules WHERE mount_id = ?1",
            params![mount_blob.as_slice()],
        )?;
        let mut position: i64 = 0;
        for pattern in includes {
            self.conn.execute(
                "INSERT INTO mount_rules (mount_id, position, kind, pattern)
                 VALUES (?1, ?2, 'include', ?3)",
                params![mount_blob.as_slice(), position, pattern.as_str()],
            )?;
            position = position.checked_add(1).ok_or(DbError::IntegerOverflow)?;
        }
        for pattern in excludes {
            self.conn.execute(
                "INSERT INTO mount_rules (mount_id, position, kind, pattern)
                 VALUES (?1, ?2, 'exclude', ?3)",
                params![mount_blob.as_slice(), position, pattern.as_str()],
            )?;
            position = position.checked_add(1).ok_or(DbError::IntegerOverflow)?;
        }
        Ok(())
    }

    pub fn mount_config(&self, mount: MountId) -> Result<Option<MountConfig>, DbError> {
        let Some(mount) = self.load_mount(mount)? else {
            return Ok(None);
        };
        Ok(Some(self.assemble_mount_config(mount)?))
    }

    pub fn list_mounts(&self, space: Option<SpaceId>) -> Result<Vec<MountConfig>, DbError> {
        let mounts = match space {
            Some(space) => {
                let bytes = space_bytes(space);
                let mut stmt = self.conn.prepare_cached(
                    "SELECT id, space_id, name FROM mounts WHERE space_id = ?1 ORDER BY name",
                )?;
                let rows = stmt.query_map(params![bytes.as_slice()], Self::map_mount_row)?;
                collect_mounts(rows)?
            }
            None => {
                let mut stmt = self
                    .conn
                    .prepare_cached("SELECT id, space_id, name FROM mounts ORDER BY name")?;
                let rows = stmt.query_map([], Self::map_mount_row)?;
                collect_mounts(rows)?
            }
        };
        let mut configs = Vec::with_capacity(mounts.len());
        for mount in mounts {
            configs.push(self.assemble_mount_config(mount)?);
        }
        Ok(configs)
    }

    pub fn entry(&self, key: &EntryKey) -> Result<Option<EntryRecord>, DbError> {
        let Some(raw) = self.load_raw_entry(key.mount, key.path.as_str())? else {
            return Ok(None);
        };
        let record = self.assemble_record(raw)?;
        if record.key.space != key.space {
            return Ok(None);
        }
        Ok(Some(record))
    }

    pub fn entries_for_mount(&self, mount: MountId) -> Result<Vec<EntryRecord>, DbError> {
        let mount_blob = mount_bytes(mount);
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {ENTRY_SELECT}
             FROM entries e
             JOIN devices d ON d.ref = e.modified_by
             JOIN mounts m ON m.id = e.mount_id
             WHERE e.mount_id = ?1
             ORDER BY e.path"
        ))?;
        let rows = stmt.query_map(params![mount_blob.as_slice()], Self::map_raw_entry)?;
        let raws = collect_raw_entries(rows)?;
        raws.into_iter()
            .map(|raw| self.assemble_record(raw))
            .collect()
    }

    /// Upsert current state (row + full replace of entry_versions) and append a history row.
    /// Unknown DeviceIds in the vector/modified_by are inserted into `devices` with name "unknown".
    /// The record's sequence must be unique.
    pub fn put_entry(&self, record: &EntryRecord) -> Result<(), DbError> {
        let Some(space) = self.mount_space(record.key.mount)? else {
            return Err(DbError::NotFound);
        };
        if space != record.key.space {
            return Err(DbError::SpaceMismatch);
        }
        self.write_entry(record)
    }

    /// Update only the local stat hint (no new version, no history row, no sequence change).
    pub fn update_stat(&self, key: &EntryKey, stat: Option<StatHint>) -> Result<(), DbError> {
        let Some(space) = self.mount_space(key.mount)? else {
            return Err(DbError::NotFound);
        };
        if space != key.space {
            return Err(DbError::SpaceMismatch);
        }
        let stat = encode_stat(stat)?;
        let mount = mount_bytes(key.mount);
        let changed = self.conn.execute(
            "UPDATE entries SET stat_size = ?1, stat_mtime_ns = ?2, stat_file_id = ?3
             WHERE mount_id = ?4 AND path = ?5",
            params![
                stat.size,
                stat.mtime_ns,
                stat.file_id,
                mount.as_slice(),
                key.path.as_str()
            ],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    pub fn changes_since(
        &self,
        after: Sequence,
        limit: usize,
    ) -> Result<Vec<EntryRecord>, DbError> {
        let after = i64_from_u64(after.0)?;
        let limit = i64::try_from(limit).map_err(|_| DbError::IntegerOverflow)?;
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {ENTRY_SELECT}
             FROM entries e
             JOIN devices d ON d.ref = e.modified_by
             JOIN mounts m ON m.id = e.mount_id
             WHERE e.sequence > ?1
             ORDER BY e.sequence
             LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![after, limit], Self::map_raw_entry)?;
        let raws = collect_raw_entries(rows)?;
        raws.into_iter()
            .map(|raw| self.assemble_record(raw))
            .collect()
    }

    pub fn history(&self, key: &EntryKey) -> Result<Vec<HistoryRecord>, DbError> {
        if let Some(space) = self.mount_space(key.mount)? {
            if space != key.space {
                return Ok(Vec::new());
            }
        }
        let Some(entry_id) = self.entry_id(key.mount, key.path.as_str())? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT h.sequence, h.kind, h.deleted, h.object_id, h.size, h.executable,
                    h.symlink_target, h.parent_object, h.vector_json, d.device_id, h.modified_at_ms
             FROM history h
             JOIN devices d ON d.ref = h.modified_by
             WHERE h.entry_id = ?1
             ORDER BY h.sequence ASC",
        )?;
        let rows = stmt.query_map(params![entry_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<Vec<u8>>>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, [u8; 32]>(9)?,
                row.get::<_, i64>(10)?,
            ))
        })?;
        let mut history = Vec::new();
        for row in rows {
            let (
                sequence,
                kind,
                deleted,
                object_id,
                size,
                executable,
                symlink_target,
                parent_object,
                vector_json,
                modified_by,
                modified_at_ms,
            ) = row?;
            let vector = serde_json::from_str(&vector_json)
                .map_err(|err| DbError::Corrupt(format!("invalid history vector_json: {err}")))?;
            history.push(HistoryRecord {
                sequence: Sequence(u64_from_i64(sequence)?),
                content: decode_content(
                    kind.as_deref(),
                    deleted,
                    object_id,
                    size,
                    executable,
                    symlink_target.as_deref(),
                )?,
                vector,
                parent_object: opt_object_id(parent_object)?,
                modified_by: DeviceId::from_bytes(modified_by),
                modified_at_unix_ms: modified_at_ms,
            });
        }
        Ok(history)
    }

    pub fn record_object(&self, id: ObjectId, size: u64, now_ms: i64) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT INTO objects (id, size, first_seen_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO NOTHING",
            params![id.as_bytes().as_slice(), i64_from_u64(size)?, now_ms],
        )?;
        Ok(())
    }

    /// Every object referenced by current entries or history (GC roots).
    pub fn live_objects(&self) -> Result<HashSet<ObjectId>, DbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT object_id FROM entries WHERE object_id IS NOT NULL
             UNION
             SELECT parent_object FROM entries WHERE parent_object IS NOT NULL
             UNION
             SELECT object_id FROM history WHERE object_id IS NOT NULL
             UNION
             SELECT parent_object FROM history WHERE parent_object IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        let mut objects = HashSet::new();
        for row in rows {
            objects.insert(object_id_from_blob(&row?)?);
        }
        Ok(objects)
    }

    pub fn object_count(&self) -> Result<u64, DbError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))?;
        u64_from_i64(count)
    }

    fn write_entry(&self, record: &EntryRecord) -> Result<(), DbError> {
        let modified_by = self.ensure_device(record.modified_by, record.modified_at_unix_ms)?;
        let mut version_refs = Vec::new();
        for (device, counter) in record.vector.iter() {
            version_refs.push((
                self.ensure_device(*device, record.modified_at_unix_ms)?,
                i64_from_u64(counter)?,
            ));
        }

        let encoded = encode_content(&record.content);
        let object_id = encoded.object_id.map(|id| *id.as_bytes());
        let size = encoded.size.map(i64_from_u64).transpose()?;
        let parent = record.parent_object.map(|id| *id.as_bytes());
        let stat = encode_stat(record.stat)?;
        let mount = mount_bytes(record.key.mount);
        let sequence = i64_from_u64(record.sequence.0)?;

        let existing = self.entry_id(record.key.mount, record.key.path.as_str())?;
        let entry_id = if let Some(entry_id) = existing {
            match self.conn.execute(
                "UPDATE entries SET
                    kind = ?1, deleted = ?2, object_id = ?3, size = ?4, executable = ?5,
                    symlink_target = ?6, parent_object = ?7, sequence = ?8, modified_by = ?9,
                    modified_at_ms = ?10, stat_size = ?11, stat_mtime_ns = ?12, stat_file_id = ?13
                 WHERE id = ?14",
                params![
                    encoded.kind,
                    encoded.deleted,
                    object_id.as_ref().map(|b| b.as_slice()),
                    size,
                    encoded.executable,
                    encoded.symlink_target.as_deref(),
                    parent.as_ref().map(|b| b.as_slice()),
                    sequence,
                    modified_by,
                    record.modified_at_unix_ms,
                    stat.size,
                    stat.mtime_ns,
                    stat.file_id,
                    entry_id,
                ],
            ) {
                Ok(_) => entry_id,
                Err(err) if is_unique_violation(&err) => {
                    return Err(DbError::DuplicateSequence(record.sequence));
                }
                Err(err) => return Err(err.into()),
            }
        } else {
            match self.conn.execute(
                "INSERT INTO entries (
                    mount_id, path, kind, deleted, object_id, size, executable, symlink_target,
                    parent_object, sequence, modified_by, modified_at_ms,
                    stat_size, stat_mtime_ns, stat_file_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    mount.as_slice(),
                    record.key.path.as_str(),
                    encoded.kind,
                    encoded.deleted,
                    object_id.as_ref().map(|b| b.as_slice()),
                    size,
                    encoded.executable,
                    encoded.symlink_target.as_deref(),
                    parent.as_ref().map(|b| b.as_slice()),
                    sequence,
                    modified_by,
                    record.modified_at_unix_ms,
                    stat.size,
                    stat.mtime_ns,
                    stat.file_id,
                ],
            ) {
                Ok(_) => self.conn.last_insert_rowid(),
                Err(err) if is_unique_violation(&err) => {
                    return Err(DbError::DuplicateSequence(record.sequence));
                }
                Err(err) => return Err(map_write_err(err, None)),
            }
        };

        self.conn.execute(
            "DELETE FROM entry_versions WHERE entry_id = ?1",
            params![entry_id],
        )?;
        for (device_ref, counter) in version_refs {
            self.conn.execute(
                "INSERT INTO entry_versions (entry_id, device_ref, counter) VALUES (?1, ?2, ?3)",
                params![entry_id, device_ref, counter],
            )?;
        }

        let vector_json = serde_json::to_string(&record.vector)?;
        self.conn.execute(
            "INSERT INTO history (
                entry_id, sequence, kind, deleted, object_id, size, executable, symlink_target,
                parent_object, vector_json, modified_by, modified_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                entry_id,
                sequence,
                encoded.kind,
                encoded.deleted,
                object_id.as_ref().map(|b| b.as_slice()),
                size,
                encoded.executable,
                encoded.symlink_target.as_deref(),
                parent.as_ref().map(|b| b.as_slice()),
                vector_json,
                modified_by,
                record.modified_at_unix_ms,
            ],
        )?;
        Ok(())
    }

    fn assemble_record(&self, raw: RawEntry) -> Result<EntryRecord, DbError> {
        let path = relay_core::LogicalPath::new(&raw.path).map_err(|err| {
            DbError::Corrupt(format!("invalid logical path {:?}: {err}", raw.path))
        })?;
        Ok(EntryRecord {
            key: EntryKey {
                space: space_from_bytes(raw.space_id),
                mount: mount_from_bytes(raw.mount_id),
                path,
            },
            content: decode_content(
                raw.kind.as_deref(),
                raw.deleted,
                raw.object_id,
                raw.size,
                raw.executable,
                raw.symlink_target.as_deref(),
            )?,
            vector: self.load_vector(raw.id)?,
            parent_object: opt_object_id(raw.parent_object)?,
            sequence: Sequence(u64_from_i64(raw.sequence)?),
            modified_by: DeviceId::from_bytes(raw.modified_by),
            modified_at_unix_ms: raw.modified_at_ms,
            stat: decode_stat(raw.stat_size, raw.stat_mtime_ns, raw.stat_file_id)?,
        })
    }

    fn load_vector(&self, entry_id: i64) -> Result<VersionVector, DbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT d.device_id, ev.counter
             FROM entry_versions ev
             JOIN devices d ON d.ref = ev.device_ref
             WHERE ev.entry_id = ?1",
        )?;
        let rows = stmt.query_map(params![entry_id], |row| {
            Ok((row.get::<_, [u8; 32]>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut vector = VersionVector::new();
        for row in rows {
            let (device, counter) = row?;
            vector.set(DeviceId::from_bytes(device), u64_from_i64(counter)?);
        }
        Ok(vector)
    }

    fn load_raw_entry(&self, mount: MountId, path: &str) -> Result<Option<RawEntry>, DbError> {
        let mount = mount_bytes(mount);
        self.conn
            .query_row(
                &format!(
                    "SELECT {ENTRY_SELECT}
                     FROM entries e
                     JOIN devices d ON d.ref = e.modified_by
                     JOIN mounts m ON m.id = e.mount_id
                     WHERE e.mount_id = ?1 AND e.path = ?2"
                ),
                params![mount.as_slice(), path],
                Self::map_raw_entry,
            )
            .optional()
            .map_err(DbError::from)
    }

    fn map_raw_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEntry> {
        Ok(RawEntry {
            id: row.get(0)?,
            mount_id: row.get(1)?,
            path: row.get(2)?,
            kind: row.get(3)?,
            deleted: row.get(4)?,
            object_id: row.get(5)?,
            size: row.get(6)?,
            executable: row.get(7)?,
            symlink_target: row.get(8)?,
            parent_object: row.get(9)?,
            sequence: row.get(10)?,
            modified_by: row.get(11)?,
            modified_at_ms: row.get(12)?,
            stat_size: row.get(13)?,
            stat_mtime_ns: row.get(14)?,
            stat_file_id: row.get(15)?,
            space_id: row.get(16)?,
        })
    }

    fn map_mount_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Mount> {
        Ok(Mount {
            id: mount_from_bytes(row.get(0)?),
            space: space_from_bytes(row.get(1)?),
            name: row.get(2)?,
        })
    }

    fn assemble_mount_config(&self, mount: Mount) -> Result<MountConfig, DbError> {
        let local_path = self.local_mount_path(mount.id)?;
        let (includes, excludes) = self.mount_rules(mount.id)?;
        Ok(MountConfig {
            mount,
            local_path,
            includes,
            excludes,
        })
    }

    fn local_mount_path(&self, mount: MountId) -> Result<Option<PathBuf>, DbError> {
        let mount = mount_bytes(mount);
        let path: Option<String> = self
            .conn
            .query_row(
                "SELECT dm.local_path
                 FROM device_mounts dm
                 JOIN local_device ld ON ld.device_ref = dm.device_ref
                 WHERE dm.mount_id = ?1 AND ld.singleton = 1",
                params![mount.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(path.map(PathBuf::from))
    }

    fn mount_rules(&self, mount: MountId) -> Result<(Vec<String>, Vec<String>), DbError> {
        let mount = mount_bytes(mount);
        let mut stmt = self.conn.prepare_cached(
            "SELECT kind, pattern FROM mount_rules WHERE mount_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map(params![mount.as_slice()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut includes = Vec::new();
        let mut excludes = Vec::new();
        for row in rows {
            let (kind, pattern) = row?;
            match kind.as_str() {
                "include" => includes.push(pattern),
                "exclude" => excludes.push(pattern),
                other => {
                    return Err(DbError::Corrupt(format!(
                        "unknown mount rule kind {other:?}"
                    )));
                }
            }
        }
        Ok((includes, excludes))
    }

    fn load_mount(&self, id: MountId) -> Result<Option<Mount>, DbError> {
        let id = mount_bytes(id);
        self.conn
            .query_row(
                "SELECT id, space_id, name FROM mounts WHERE id = ?1",
                params![id.as_slice()],
                Self::map_mount_row,
            )
            .optional()
            .map_err(DbError::from)
    }

    fn mount_space(&self, id: MountId) -> Result<Option<SpaceId>, DbError> {
        Ok(self.load_mount(id)?.map(|mount| mount.space))
    }

    fn entry_id(&self, mount: MountId, path: &str) -> Result<Option<i64>, DbError> {
        let mount = mount_bytes(mount);
        self.conn
            .query_row(
                "SELECT id FROM entries WHERE mount_id = ?1 AND path = ?2",
                params![mount.as_slice(), path],
                |row| row.get(0),
            )
            .optional()
            .map_err(DbError::from)
    }

    fn device_ref(&self, id: DeviceId) -> Result<Option<i64>, DbError> {
        let id = device_id_bytes(id);
        self.conn
            .query_row(
                "SELECT ref FROM devices WHERE device_id = ?1",
                params![id.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(DbError::from)
    }

    fn ensure_device(&self, id: DeviceId, now_ms: i64) -> Result<i64, DbError> {
        if let Some(existing) = self.device_ref(id)? {
            return Ok(existing);
        }
        let bytes = device_id_bytes(id);
        self.conn.execute(
            "INSERT INTO devices (device_id, name, status, created_at_ms, last_seen_ms)
             VALUES (?1, 'unknown', 'active', ?2, ?2)",
            params![bytes.as_slice(), now_ms],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
}

fn collect_mounts(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<Mount>>,
) -> Result<Vec<Mount>, DbError> {
    let mut mounts = Vec::new();
    for row in rows {
        mounts.push(row?);
    }
    Ok(mounts)
}

fn collect_raw_entries(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<RawEntry>>,
) -> Result<Vec<RawEntry>, DbError> {
    let mut raws = Vec::new();
    for row in rows {
        raws.push(row?);
    }
    Ok(raws)
}
