use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub use relay_core::DeleteHoldDecision;
use relay_core::{
    Device, DeviceId, EntryContent, EntryKey, EntryRecord, MaterializationRuleId, Mount, MountId,
    ObjectId, PolicyId, Sequence, Space, SpaceId, StatHint, VersionVector,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::DbError;
use crate::convert::{
    decode_content, decode_stat, device_id_bytes, encode_content, encode_stat, i64_from_u64,
    is_unique_violation, map_write_err, materialization_rule_bytes,
    materialization_rule_from_bytes, mount_bytes, mount_from_bytes, object_id_from_blob,
    opt_object_id, policy_bytes, policy_from_bytes, space_bytes, space_from_bytes, u64_from_i64,
};

const ENTRY_SELECT: &str = "e.id, e.mount_id, e.path, e.kind, e.deleted, e.object_id, e.size,
     e.executable, e.symlink_target, e.parent_object, e.sequence,
     d.device_id, e.modified_at_ms, e.stat_size, e.stat_mtime_ns, e.stat_file_id,
     e.stat_ctime_ns, m.space_id, e.materialized";

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
pub struct PeerRecord {
    pub device: Device,
    pub addresses: Vec<String>,
    pub added_at_ms: i64,
    /// Wall time of the last live session with this device. `None` until the
    /// first connection. While the peer is offline this is when that session
    /// ended (or the last presence refresh, if the session did not end cleanly).
    pub last_seen_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OfferedMount {
    pub id: MountId,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OfferedMember {
    pub id: DeviceId,
    pub name: String,
    pub addresses: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerOfferRow {
    pub space_id: SpaceId,
    pub name: String,
    pub mounts: Vec<OfferedMount>,
    pub members: Vec<OfferedMember>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredOffer {
    pub peer: Device,
    pub space_id: SpaceId,
    pub name: String,
    pub mounts: Vec<OfferedMount>,
    pub members: Vec<OfferedMember>,
    pub received_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncProgress {
    pub received_seq: Sequence,
    pub acked_seq: Sequence,
    pub last_sync_ms: Option<i64>,
}

/// How much of a space is still unseen by a peer that has applied through `after`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatchupPlan {
    pub files: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceGroupRecord {
    pub name: String,
    pub members: Vec<DeviceId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyRecord {
    pub id: PolicyId,
    pub space_id: SpaceId,
    pub name: String,
    pub selectors: Vec<String>,
    /// Direct device targets (not expanded from groups).
    pub peer_targets: Vec<DeviceId>,
    pub group_targets: Vec<String>,
}

/// An index row that has no working-tree copy. Hydration reads only these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexOnlyEntry {
    pub space: SpaceId,
    pub mount: MountId,
    pub mount_name: String,
    pub path: relay_core::LogicalPath,
    pub object: Option<ObjectId>,
}

/// A local materialization rule. Selectors are ordered by their own position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializationRuleRecord {
    pub id: MaterializationRuleId,
    pub space_id: SpaceId,
    pub name: String,
    pub mode: String,
    pub position: i64,
    pub selectors: Vec<String>,
}

/// One policy as stored in a peer's snapshot (targets already expanded).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SnapshotPolicy {
    pub id: PolicyId,
    pub name: String,
    pub selectors: Vec<String>,
    pub targets: Vec<DeviceId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerPolicySnapshot {
    pub epoch: u64,
    pub policies: Vec<SnapshotPolicy>,
}

fn parse_decision(value: &str) -> Result<DeleteHoldDecision, DbError> {
    DeleteHoldDecision::parse(value)
        .ok_or_else(|| DbError::Corrupt(format!("unknown delete hold decision {value:?}")))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteHoldRow {
    pub peer: Device,
    pub space: Space,
    pub mount: Mount,
    pub deletions: usize,
    pub live: usize,
    pub held_at_ms: i64,
    pub decision: Option<DeleteHoldDecision>,
    pub decided_at_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountState {
    pub last_scan_ms: Option<i64>,
    pub last_full_scan_ms: Option<i64>,
    pub last_error: Option<String>,
    pub last_error_ms: Option<i64>,
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
    stat_ctime_ns: Option<i64>,
    space_id: [u8; 16],
    materialized: i64,
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

    pub fn local_setting(&self, key: &str) -> Result<Option<String>, DbError> {
        self.conn
            .query_row(
                "SELECT value FROM local_settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(DbError::from)
    }

    pub fn set_local_setting(&self, key: &str, value: &str) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT INTO local_settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn clear_local_setting(&self, key: &str) -> Result<(), DbError> {
        self.conn
            .execute("DELETE FROM local_settings WHERE key = ?1", params![key])?;
        Ok(())
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

    /// Insert a device or rename it without treating that as a live sighting.
    /// New rows keep `last_seen_ms` null until [`Self::touch_last_seen`].
    fn upsert_device_name(&self, device: &Device, created_at_ms: i64) -> Result<(), DbError> {
        let id = device_id_bytes(device.id);
        self.conn.execute(
            "INSERT INTO devices (device_id, name, status, created_at_ms, last_seen_ms)
             VALUES (?1, ?2, 'active', ?3, NULL)
             ON CONFLICT(device_id) DO UPDATE SET
                name = excluded.name",
            params![id.as_slice(), device.name.as_str(), created_at_ms],
        )?;
        Ok(())
    }

    /// Record that `id` was reachable at `now_ms`.
    pub fn touch_last_seen(&self, id: DeviceId, now_ms: i64) -> Result<(), DbError> {
        let id = device_id_bytes(id);
        let updated = self.conn.execute(
            "UPDATE devices SET last_seen_ms = ?1 WHERE device_id = ?2",
            params![now_ms, id.as_slice()],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound);
        }
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
        let device_ref = self.local_device_ref()?;
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

    /// Return a mount to the unattached state an offered mount starts in.
    ///
    /// An unattached mount stores no entries (D16), so its index rows and
    /// history go too. Keeping them would make a later attach to a different
    /// folder look like every file was deleted. The `mounts` row stays: the
    /// mount still exists in the space on other devices.
    pub fn detach_local_mount(&self, mount: MountId) -> Result<(), DbError> {
        let device_ref = self.local_device_ref()?;
        let mount = mount_bytes(mount);
        let mount = mount.as_slice();
        for sql in [
            "DELETE FROM entries WHERE mount_id = ?1",
            "DELETE FROM delete_holds WHERE mount_id = ?1",
            "DELETE FROM mount_state WHERE mount_id = ?1",
            "DELETE FROM mount_rules WHERE mount_id = ?1",
        ] {
            self.conn.execute(sql, params![mount])?;
        }
        self.conn.execute(
            "DELETE FROM device_mounts WHERE device_ref = ?1 AND mount_id = ?2",
            params![device_ref, mount],
        )?;
        Ok(())
    }

    /// Remove a space and everything scoped to it on this device. The caller
    /// detaches its mounts first.
    ///
    /// Kept on purpose: `peer_offers`, so the space can be joined again, and
    /// `space_key_wraps`, so a rejoin can still open mailbox objects sealed
    /// under earlier key generations (D30).
    pub fn delete_space(&self, space: SpaceId) -> Result<(), DbError> {
        let space = space_bytes(space);
        let space = space.as_slice();
        for sql in [
            "DELETE FROM space_shares WHERE space_id = ?1",
            "DELETE FROM sync_progress WHERE space_id = ?1",
            "DELETE FROM delete_holds WHERE space_id = ?1",
            "DELETE FROM peer_policy_snapshots WHERE space_id = ?1",
            "DELETE FROM replication_policies WHERE space_id = ?1",
            "DELETE FROM replica_push WHERE space_id = ?1",
            "DELETE FROM materialization_rules WHERE space_id = ?1",
            "DELETE FROM mount_state WHERE mount_id IN (SELECT id FROM mounts WHERE space_id = ?1)",
            "DELETE FROM mount_rules WHERE mount_id IN (SELECT id FROM mounts WHERE space_id = ?1)",
            "DELETE FROM mounts WHERE space_id = ?1",
        ] {
            self.conn.execute(sql, params![space])?;
        }
        if self
            .conn
            .execute("DELETE FROM spaces WHERE id = ?1", params![space])?
            == 0
        {
            return Err(DbError::NotFound);
        }
        Ok(())
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

    /// Entries at `prefix` and every path under it (`prefix/...`).
    ///
    /// Uses a range query rather than `LIKE`, so `%` and `_` in names are
    /// literals. The upper bound is `prefix` plus the byte after `'/'`.
    pub fn entries_under(
        &self,
        mount: MountId,
        prefix: &relay_core::LogicalPath,
    ) -> Result<Vec<EntryRecord>, DbError> {
        let mount_blob = mount_bytes(mount);
        let exact = prefix.as_str();
        let lower = format!("{exact}/");
        let upper = format!("{exact}0");
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {ENTRY_SELECT}
             FROM entries e
             JOIN devices d ON d.ref = e.modified_by
             JOIN mounts m ON m.id = e.mount_id
             WHERE e.mount_id = ?1
               AND (e.path = ?2 OR (e.path >= ?3 AND e.path < ?4))
             ORDER BY e.path"
        ))?;
        let rows = stmt.query_map(
            params![mount_blob.as_slice(), exact, lower, upper],
            Self::map_raw_entry,
        )?;
        let raws = collect_raw_entries(rows)?;
        raws.into_iter()
            .map(|raw| self.assemble_record(raw))
            .collect()
    }

    pub fn count_live(&self, mount: MountId) -> Result<usize, DbError> {
        let mount_blob = mount_bytes(mount);
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE mount_id = ?1 AND deleted = 0",
            params![mount_blob.as_slice()],
            |row| row.get(0),
        )?;
        usize::try_from(count).map_err(|_| DbError::IntegerOverflow)
    }

    /// Upsert current state (row + full replace of entry_versions) and append a history row.
    /// Unknown DeviceIds in the vector/modified_by are inserted into `devices` with
    /// a placeholder name equal to the short id.
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
            "UPDATE entries SET stat_size = ?1, stat_mtime_ns = ?2, stat_file_id = ?3,
                 stat_ctime_ns = ?4
             WHERE mount_id = ?5 AND path = ?6",
            params![
                stat.size,
                stat.mtime_ns,
                stat.file_id,
                stat.ctime_ns,
                mount.as_slice(),
                key.path.as_str()
            ],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    /// Live entries this device has not written (`materialized = 0`), on mounts
    /// attached locally.
    pub fn list_index_only(&self) -> Result<Vec<IndexOnlyEntry>, DbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT m.space_id, m.id, m.name, e.path, e.object_id
             FROM entries e
             JOIN mounts m ON m.id = e.mount_id
             JOIN device_mounts dm ON dm.mount_id = m.id
             JOIN local_device ld ON ld.device_ref = dm.device_ref
             WHERE e.materialized = 0 AND e.deleted = 0
             ORDER BY m.name, e.path",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, [u8; 16]>(0)?,
                row.get::<_, [u8; 16]>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (space, mount, mount_name, path, object) = row?;
            let path = relay_core::LogicalPath::new(&path)
                .map_err(|err| DbError::Corrupt(format!("invalid logical path {path:?}: {err}")))?;
            out.push(IndexOnlyEntry {
                space: space_from_bytes(space),
                mount: mount_from_bytes(mount),
                mount_name,
                path,
                object: object.as_deref().map(object_id_from_blob).transpose()?,
            });
        }
        Ok(out)
    }

    /// Flip the local materialization flag without a new version or vector bump.
    pub fn set_materialized(
        &self,
        key: &EntryKey,
        materialized: bool,
        stat: Option<StatHint>,
    ) -> Result<(), DbError> {
        let Some(space) = self.mount_space(key.mount)? else {
            return Err(DbError::NotFound);
        };
        if space != key.space {
            return Err(DbError::SpaceMismatch);
        }
        let stat = encode_stat(stat)?;
        let mount = mount_bytes(key.mount);
        let flag: i64 = if materialized { 1 } else { 0 };
        let changed = self.conn.execute(
            "UPDATE entries SET materialized = ?1, stat_size = ?2, stat_mtime_ns = ?3,
                 stat_file_id = ?4, stat_ctime_ns = ?5
             WHERE mount_id = ?6 AND path = ?7",
            params![
                flag,
                stat.size,
                stat.mtime_ns,
                stat.file_id,
                stat.ctime_ns,
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
        if let Some(space) = self.mount_space(key.mount)?
            && space != key.space
        {
            return Ok(Vec::new());
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

    /// Objects this device stored, plus file entries whose working tree is present.
    ///
    /// History-only references, parent pointers, and `materialized = 0` rows are
    /// not required to exist in the local store.
    pub fn verification_objects(&self) -> Result<HashSet<ObjectId>, DbError> {
        let mut objects = HashSet::new();
        let mut stored = self.conn.prepare_cached("SELECT id FROM objects")?;
        let rows = stored.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        for row in rows {
            objects.insert(object_id_from_blob(&row?)?);
        }
        let mut live = self.conn.prepare_cached(
            "SELECT object_id FROM entries
             WHERE materialized = 1 AND deleted = 0 AND object_id IS NOT NULL",
        )?;
        let rows = live.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        for row in rows {
            objects.insert(object_id_from_blob(&row?)?);
        }
        Ok(objects)
    }

    pub fn record_scan_success(
        &self,
        mount: MountId,
        full: bool,
        now_ms: i64,
    ) -> Result<(), DbError> {
        if self.mount_space(mount)?.is_none() {
            return Err(DbError::NotFound);
        }
        let mount = mount_bytes(mount);
        let full_ms = full.then_some(now_ms);
        self.conn.execute(
            "INSERT INTO mount_state (mount_id, last_scan_ms, last_full_scan_ms, last_error, last_error_ms)
             VALUES (?1, ?2, ?3, NULL, NULL)
             ON CONFLICT(mount_id) DO UPDATE SET
                last_scan_ms = excluded.last_scan_ms,
                last_full_scan_ms = COALESCE(excluded.last_full_scan_ms, mount_state.last_full_scan_ms),
                last_error = NULL,
                last_error_ms = NULL",
            params![mount.as_slice(), now_ms, full_ms],
        )?;
        Ok(())
    }

    pub fn record_scan_error(
        &self,
        mount: MountId,
        message: &str,
        now_ms: i64,
    ) -> Result<(), DbError> {
        if self.mount_space(mount)?.is_none() {
            return Err(DbError::NotFound);
        }
        let mount = mount_bytes(mount);
        self.conn.execute(
            "INSERT INTO mount_state (mount_id, last_scan_ms, last_full_scan_ms, last_error, last_error_ms)
             VALUES (?1, NULL, NULL, ?2, ?3)
             ON CONFLICT(mount_id) DO UPDATE SET
                last_error = excluded.last_error,
                last_error_ms = excluded.last_error_ms",
            params![mount.as_slice(), message, now_ms],
        )?;
        Ok(())
    }

    pub fn mount_state(&self, mount: MountId) -> Result<Option<MountState>, DbError> {
        let mount = mount_bytes(mount);
        self.conn
            .query_row(
                "SELECT last_scan_ms, last_full_scan_ms, last_error, last_error_ms
                 FROM mount_state WHERE mount_id = ?1",
                params![mount.as_slice()],
                |row| {
                    Ok(MountState {
                        last_scan_ms: row.get(0)?,
                        last_full_scan_ms: row.get(1)?,
                        last_error: row.get(2)?,
                        last_error_ms: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(DbError::from)
    }

    pub fn object_count(&self) -> Result<u64, DbError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))?;
        u64_from_i64(count)
    }

    pub fn latest_sequence(&self) -> Result<Sequence, DbError> {
        let local = self.local_device()?.ok_or(DbError::NotInitialized)?;
        Ok(Sequence(local.next_sequence.0.saturating_sub(1)))
    }

    pub fn max_sequence_in_space(&self, space: SpaceId) -> Result<Sequence, DbError> {
        let bytes = space_bytes(space);
        let max: Option<i64> = self.conn.query_row(
            "SELECT MAX(e.sequence) FROM entries e
             JOIN mounts m ON m.id = e.mount_id
             WHERE m.space_id = ?1",
            params![bytes.as_slice()],
            |row| row.get(0),
        )?;
        Ok(Sequence(max.map(u64_from_i64).transpose()?.unwrap_or(0)))
    }

    /// Entries and file bytes in this space with `sequence > after`.
    ///
    /// `files` counts every entry, including directories and deletes. `bytes`
    /// sums sizes of live files only.
    pub fn catchup_plan(&self, space: SpaceId, after: Sequence) -> Result<CatchupPlan, DbError> {
        let space = space_bytes(space);
        let after = i64_from_u64(after.0)?;
        let (files, bytes): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE
                        WHEN e.kind = 'file' AND e.deleted = 0 THEN COALESCE(e.size, 0)
                        ELSE 0
                    END), 0)
             FROM entries e
             JOIN mounts m ON m.id = e.mount_id
             WHERE m.space_id = ?1 AND e.sequence > ?2",
            params![space.as_slice(), after],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(CatchupPlan {
            files: u64_from_i64(files)?,
            bytes: u64_from_i64(bytes)?,
        })
    }

    pub fn changes_since_in_space(
        &self,
        space: SpaceId,
        after: Sequence,
        limit: usize,
    ) -> Result<Vec<EntryRecord>, DbError> {
        let space = space_bytes(space);
        let after = i64_from_u64(after.0)?;
        let limit = i64::try_from(limit).map_err(|_| DbError::IntegerOverflow)?;
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {ENTRY_SELECT}
             FROM entries e
             JOIN devices d ON d.ref = e.modified_by
             JOIN mounts m ON m.id = e.mount_id
             WHERE m.space_id = ?1 AND e.sequence > ?2
             ORDER BY e.sequence
             LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![space.as_slice(), after, limit], Self::map_raw_entry)?;
        let raws = collect_raw_entries(rows)?;
        raws.into_iter()
            .map(|raw| self.assemble_record(raw))
            .collect()
    }

    pub fn add_peer(
        &self,
        device: &Device,
        addresses: &[String],
        now_ms: i64,
    ) -> Result<PeerRecord, DbError> {
        self.upsert_device_name(device, now_ms)?;
        let device_ref = self
            .device_ref(device.id)?
            .ok_or_else(|| DbError::Corrupt("upserted peer device is missing".into()))?;
        let addresses_json = serde_json::to_string(addresses)?;
        match self.conn.execute(
            "INSERT INTO peers (device_ref, name, addresses, added_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![device_ref, device.name.as_str(), addresses_json, now_ms],
        ) {
            Ok(_) => {}
            Err(err) => return Err(map_write_err(err, Some(&device.name))),
        }
        let id_bytes = device_id_bytes(device.id);
        self.conn.execute(
            "DELETE FROM dismissed_peers WHERE device_id = ?1",
            params![id_bytes.as_slice()],
        )?;
        self.peer_by_id(device.id)?
            .ok_or_else(|| DbError::Corrupt("inserted peer is missing".into()))
    }

    pub fn update_peer(
        &self,
        id: DeviceId,
        name: &str,
        addresses: &[String],
        now_ms: i64,
    ) -> Result<PeerRecord, DbError> {
        self.upsert_device_name(
            &Device {
                id,
                name: name.to_owned(),
            },
            now_ms,
        )?;
        let device_ref = self
            .device_ref(id)?
            .ok_or_else(|| DbError::Corrupt("upserted peer device is missing".into()))?;
        let addresses_json = serde_json::to_string(addresses)?;
        self.conn.execute(
            "UPDATE peers SET name = ?1, addresses = ?2 WHERE device_ref = ?3",
            params![name, addresses_json, device_ref],
        )?;
        let id_bytes = device_id_bytes(id);
        self.conn.execute(
            "DELETE FROM dismissed_peers WHERE device_id = ?1",
            params![id_bytes.as_slice()],
        )?;
        self.peer_by_id(id)?.ok_or(DbError::NotFound)
    }

    pub fn remove_peer_by_name(&self, name: &str) -> Result<bool, DbError> {
        let Some(peer) = self.peer_by_name(name)? else {
            return Ok(false);
        };
        let device_ref = self
            .device_ref(peer.device.id)?
            .ok_or_else(|| DbError::Corrupt("peer device is missing".into()))?;
        let id_bytes = device_id_bytes(peer.device.id);
        self.conn.execute(
            "INSERT OR IGNORE INTO dismissed_peers (device_id) VALUES (?1)",
            params![id_bytes.as_slice()],
        )?;
        self.conn.execute(
            "DELETE FROM space_shares WHERE device_ref = ?1",
            params![device_ref],
        )?;
        self.conn.execute(
            "DELETE FROM peer_offers WHERE device_ref = ?1",
            params![device_ref],
        )?;
        self.conn.execute(
            "DELETE FROM sync_progress WHERE device_ref = ?1",
            params![device_ref],
        )?;
        self.conn.execute(
            "DELETE FROM delete_holds WHERE device_ref = ?1",
            params![device_ref],
        )?;
        self.conn.execute(
            "DELETE FROM peers WHERE device_ref = ?1",
            params![device_ref],
        )?;
        Ok(true)
    }

    pub fn is_dismissed(&self, id: DeviceId) -> Result<bool, DbError> {
        let bytes = device_id_bytes(id);
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM dismissed_peers WHERE device_id = ?1",
            params![bytes.as_slice()],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn peer_by_name(&self, name: &str) -> Result<Option<PeerRecord>, DbError> {
        self.load_peer("p.name = ?1", params![name])
    }

    pub fn peer_by_id(&self, id: DeviceId) -> Result<Option<PeerRecord>, DbError> {
        let bytes = device_id_bytes(id);
        self.load_peer("d.device_id = ?1", params![bytes.as_slice()])
    }

    pub fn list_peers(&self) -> Result<Vec<PeerRecord>, DbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT d.device_id, p.name, p.addresses, p.added_at_ms, d.last_seen_ms
             FROM peers p
             JOIN devices d ON d.ref = p.device_ref
             ORDER BY p.name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, [u8; 32]>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })?;
        let mut peers = Vec::new();
        for row in rows {
            let (id, name, addresses, added_at_ms, last_seen_ms) = row?;
            peers.push(parse_peer(id, name, addresses, added_at_ms, last_seen_ms)?);
        }
        Ok(peers)
    }

    pub fn share_space(&self, space: SpaceId, peer: DeviceId) -> Result<(), DbError> {
        let space = space_bytes(space);
        let device_ref = self.device_ref(peer)?.ok_or(DbError::NotFound)?;
        match self.conn.execute(
            "INSERT OR IGNORE INTO space_shares (space_id, device_ref) VALUES (?1, ?2)",
            params![space.as_slice(), device_ref],
        ) {
            Ok(_) => Ok(()),
            Err(err) => Err(map_write_err(err, None)),
        }
    }

    pub fn unshare_space(&self, space: SpaceId, peer: DeviceId) -> Result<(), DbError> {
        let space = space_bytes(space);
        let Some(device_ref) = self.device_ref(peer)? else {
            return Ok(());
        };
        self.conn.execute(
            "DELETE FROM space_shares WHERE space_id = ?1 AND device_ref = ?2",
            params![space.as_slice(), device_ref],
        )?;
        Ok(())
    }

    pub fn is_shared(&self, space: SpaceId, peer: DeviceId) -> Result<bool, DbError> {
        let space = space_bytes(space);
        let Some(device_ref) = self.device_ref(peer)? else {
            return Ok(false);
        };
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM space_shares WHERE space_id = ?1 AND device_ref = ?2",
            params![space.as_slice(), device_ref],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn shared_space_ids(&self, peer: DeviceId) -> Result<Vec<SpaceId>, DbError> {
        let Some(device_ref) = self.device_ref(peer)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self
            .conn
            .prepare_cached("SELECT space_id FROM space_shares WHERE device_ref = ?1")?;
        let rows = stmt.query_map(params![device_ref], |row| row.get::<_, [u8; 16]>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(space_from_bytes(row?));
        }
        Ok(ids)
    }

    pub fn policy_epoch(&self, space: SpaceId) -> Result<u64, DbError> {
        let bytes = space_bytes(space);
        let epoch: i64 = self
            .conn
            .query_row(
                "SELECT policy_epoch FROM spaces WHERE id = ?1",
                params![bytes.as_slice()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(DbError::NotFound)?;
        u64_from_i64(epoch)
    }

    pub fn bump_policy_epoch(&self, space: SpaceId) -> Result<u64, DbError> {
        let bytes = space_bytes(space);
        let updated = self.conn.execute(
            "UPDATE spaces SET policy_epoch = policy_epoch + 1 WHERE id = ?1",
            params![bytes.as_slice()],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound);
        }
        self.policy_epoch(space)
    }

    pub fn create_device_group(&self, name: &str, now_ms: i64) -> Result<(), DbError> {
        match self.conn.execute(
            "INSERT INTO device_groups (name, created_at_ms) VALUES (?1, ?2)",
            params![name, now_ms],
        ) {
            Ok(_) => Ok(()),
            Err(err) => Err(map_write_err(err, Some(name))),
        }
    }

    pub fn delete_device_group(&self, name: &str) -> Result<(), DbError> {
        // Detach from policies first so removal is correct even without CASCADE.
        self.conn.execute(
            "DELETE FROM replication_policy_groups WHERE group_name = ?1",
            params![name],
        )?;
        self.conn.execute(
            "DELETE FROM device_group_members WHERE group_name = ?1",
            params![name],
        )?;
        let n = self
            .conn
            .execute("DELETE FROM device_groups WHERE name = ?1", params![name])?;
        if n == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    pub fn add_group_member(&self, group: &str, device: DeviceId) -> Result<(), DbError> {
        let exists: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM device_groups WHERE name = ?1",
            params![group],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Err(DbError::NotFound);
        }
        let id = device_id_bytes(device);
        match self.conn.execute(
            "INSERT OR IGNORE INTO device_group_members (group_name, device_id) VALUES (?1, ?2)",
            params![group, id.as_slice()],
        ) {
            Ok(_) => Ok(()),
            Err(err) => Err(map_write_err(err, Some(group))),
        }
    }

    pub fn remove_group_member(&self, group: &str, device: DeviceId) -> Result<(), DbError> {
        let exists: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM device_groups WHERE name = ?1",
            params![group],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Err(DbError::NotFound);
        }
        let id = device_id_bytes(device);
        self.conn.execute(
            "DELETE FROM device_group_members WHERE group_name = ?1 AND device_id = ?2",
            params![group, id.as_slice()],
        )?;
        Ok(())
    }

    pub fn list_device_groups(&self) -> Result<Vec<DeviceGroupRecord>, DbError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT name FROM device_groups ORDER BY name")?;
        let names: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            out.push(DeviceGroupRecord {
                members: self.group_members(&name)?,
                name,
            });
        }
        Ok(out)
    }

    pub fn group_members(&self, group: &str) -> Result<Vec<DeviceId>, DbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT device_id FROM device_group_members WHERE group_name = ?1 ORDER BY device_id",
        )?;
        let rows = stmt.query_map(params![group], |row| row.get::<_, [u8; 32]>(0))?;
        let mut members = Vec::new();
        for row in rows {
            members.push(DeviceId::from_bytes(row?));
        }
        Ok(members)
    }

    /// Spaces that have a local policy referencing `group`.
    pub fn spaces_referencing_group(&self, group: &str) -> Result<Vec<SpaceId>, DbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT p.space_id
             FROM replication_policies p
             JOIN replication_policy_groups g ON g.policy_id = p.id
             WHERE g.group_name = ?1",
        )?;
        let rows = stmt.query_map(params![group], |row| row.get::<_, [u8; 16]>(0))?;
        let mut spaces = Vec::new();
        for row in rows {
            spaces.push(space_from_bytes(row?));
        }
        Ok(spaces)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_policy(
        &self,
        id: PolicyId,
        space: SpaceId,
        name: &str,
        selectors: &[String],
        peers: &[DeviceId],
        groups: &[String],
        now_ms: i64,
    ) -> Result<(), DbError> {
        let id_bytes = policy_bytes(id);
        let space_bytes = space_bytes(space);
        let selectors_json = serde_json::to_string(selectors)?;
        match self.conn.execute(
            "INSERT INTO replication_policies (id, space_id, name, selectors_json, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id_bytes.as_slice(),
                space_bytes.as_slice(),
                name,
                selectors_json,
                now_ms
            ],
        ) {
            Ok(_) => {}
            Err(err) => return Err(map_write_err(err, Some(name))),
        }
        for peer in peers {
            let peer_bytes = device_id_bytes(*peer);
            self.conn.execute(
                "INSERT INTO replication_policy_peers (policy_id, device_id) VALUES (?1, ?2)",
                params![id_bytes.as_slice(), peer_bytes.as_slice()],
            )?;
        }
        for group in groups {
            let exists: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM device_groups WHERE name = ?1",
                params![group.as_str()],
                |row| row.get(0),
            )?;
            if exists == 0 {
                return Err(DbError::NotFound);
            }
            self.conn.execute(
                "INSERT INTO replication_policy_groups (policy_id, group_name) VALUES (?1, ?2)",
                params![id_bytes.as_slice(), group.as_str()],
            )?;
        }
        Ok(())
    }

    pub fn delete_policy(&self, space: SpaceId, name: &str) -> Result<(), DbError> {
        let Some(policy) = self.policy_by_name(space, name)? else {
            return Err(DbError::NotFound);
        };
        let id = policy_bytes(policy.id);
        self.conn.execute(
            "DELETE FROM replication_policy_peers WHERE policy_id = ?1",
            params![id.as_slice()],
        )?;
        self.conn.execute(
            "DELETE FROM replication_policy_groups WHERE policy_id = ?1",
            params![id.as_slice()],
        )?;
        self.conn.execute(
            "DELETE FROM replication_policies WHERE id = ?1",
            params![id.as_slice()],
        )?;
        Ok(())
    }

    pub fn policy_by_name(
        &self,
        space: SpaceId,
        name: &str,
    ) -> Result<Option<PolicyRecord>, DbError> {
        let space_bytes = space_bytes(space);
        let row = self
            .conn
            .query_row(
                "SELECT id, space_id, name, selectors_json FROM replication_policies
                 WHERE space_id = ?1 AND name = ?2",
                params![space_bytes.as_slice(), name],
                |row| {
                    Ok((
                        row.get::<_, [u8; 16]>(0)?,
                        row.get::<_, [u8; 16]>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        match row {
            Some((id, space_id, name, selectors_json)) => Ok(Some(self.load_policy(
                id,
                space_id,
                name,
                selectors_json,
            )?)),
            None => Ok(None),
        }
    }

    pub fn list_policies(&self, space: SpaceId) -> Result<Vec<PolicyRecord>, DbError> {
        let space_bytes = space_bytes(space);
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, space_id, name, selectors_json FROM replication_policies
             WHERE space_id = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map(params![space_bytes.as_slice()], |row| {
            Ok((
                row.get::<_, [u8; 16]>(0)?,
                row.get::<_, [u8; 16]>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, space_id, name, selectors_json) = row?;
            out.push(self.load_policy(id, space_id, name, selectors_json)?);
        }
        Ok(out)
    }

    /// Expanded device targets for a local policy (direct peers ∪ group members).
    pub fn expand_policy_targets(&self, policy: &PolicyRecord) -> Result<Vec<DeviceId>, DbError> {
        let mut set = HashSet::new();
        for peer in &policy.peer_targets {
            set.insert(*peer);
        }
        for group in &policy.group_targets {
            for member in self.group_members(group)? {
                set.insert(member);
            }
        }
        let mut targets: Vec<_> = set.into_iter().collect();
        targets.sort();
        Ok(targets)
    }

    pub fn put_peer_policy_snapshot(
        &self,
        peer: DeviceId,
        space: SpaceId,
        epoch: u64,
        policies: &[SnapshotPolicy],
    ) -> Result<(), DbError> {
        let device_ref = self.device_ref(peer)?.ok_or(DbError::NotFound)?;
        let space_bytes = space_bytes(space);
        let epoch = i64_from_u64(epoch)?;
        let policies_json = serde_json::to_string(policies)?;
        self.conn.execute(
            "INSERT INTO peer_policy_snapshots (device_ref, space_id, epoch, policies_json)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(device_ref, space_id) DO UPDATE SET
                epoch = excluded.epoch,
                policies_json = excluded.policies_json",
            params![device_ref, space_bytes.as_slice(), epoch, policies_json],
        )?;
        Ok(())
    }

    pub fn peer_policy_snapshot(
        &self,
        peer: DeviceId,
        space: SpaceId,
    ) -> Result<Option<PeerPolicySnapshot>, DbError> {
        let Some(device_ref) = self.device_ref(peer)? else {
            return Ok(None);
        };
        let space_bytes = space_bytes(space);
        self.conn
            .query_row(
                "SELECT epoch, policies_json FROM peer_policy_snapshots
                 WHERE device_ref = ?1 AND space_id = ?2",
                params![device_ref, space_bytes.as_slice()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(epoch, policies_json)| {
                Ok(PeerPolicySnapshot {
                    epoch: u64_from_i64(epoch)?,
                    policies: serde_json::from_str(&policies_json)?,
                })
            })
            .transpose()
    }

    /// All snapshot policies for a space from every peer, plus nothing about
    /// local policies (caller unions those separately).
    pub fn all_peer_policy_snapshots_for_space(
        &self,
        space: SpaceId,
    ) -> Result<Vec<SnapshotPolicy>, DbError> {
        let space_bytes = space_bytes(space);
        let mut stmt = self.conn.prepare_cached(
            "SELECT policies_json FROM peer_policy_snapshots WHERE space_id = ?1",
        )?;
        let rows = stmt.query_map(params![space_bytes.as_slice()], |row| {
            row.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for row in rows {
            let policies: Vec<SnapshotPolicy> = serde_json::from_str(&row?)?;
            out.extend(policies);
        }
        Ok(out)
    }

    fn load_policy(
        &self,
        id: [u8; 16],
        space_id: [u8; 16],
        name: String,
        selectors_json: String,
    ) -> Result<PolicyRecord, DbError> {
        let policy_id = policy_from_bytes(id);
        let id_bytes = policy_bytes(policy_id);
        let selectors: Vec<String> = serde_json::from_str(&selectors_json)?;
        let mut peer_stmt = self.conn.prepare_cached(
            "SELECT device_id FROM replication_policy_peers WHERE policy_id = ?1 ORDER BY device_id",
        )?;
        let peer_rows = peer_stmt.query_map(params![id_bytes.as_slice()], |row| {
            row.get::<_, [u8; 32]>(0)
        })?;
        let mut peer_targets = Vec::new();
        for row in peer_rows {
            peer_targets.push(DeviceId::from_bytes(row?));
        }
        let mut group_stmt = self.conn.prepare_cached(
            "SELECT group_name FROM replication_policy_groups WHERE policy_id = ?1 ORDER BY group_name",
        )?;
        let group_rows =
            group_stmt.query_map(params![id_bytes.as_slice()], |row| row.get::<_, String>(0))?;
        let mut group_targets = Vec::new();
        for row in group_rows {
            group_targets.push(row?);
        }
        Ok(PolicyRecord {
            id: policy_id,
            space_id: space_from_bytes(space_id),
            name,
            selectors,
            peer_targets,
            group_targets,
        })
    }

    pub fn replace_peer_offers(
        &self,
        peer: DeviceId,
        offers: &[PeerOfferRow],
        now_ms: i64,
    ) -> Result<(), DbError> {
        let device_ref = self.device_ref(peer)?.ok_or(DbError::NotFound)?;
        self.conn.execute(
            "DELETE FROM peer_offers WHERE device_ref = ?1",
            params![device_ref],
        )?;
        for offer in offers {
            let space = space_bytes(offer.space_id);
            let mounts_json = serde_json::to_string(&offer.mounts)?;
            let members_json = serde_json::to_string(&offer.members)?;
            self.conn.execute(
                "INSERT INTO peer_offers (device_ref, space_id, name, mounts_json, members_json, received_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    device_ref,
                    space.as_slice(),
                    offer.name.as_str(),
                    mounts_json,
                    members_json,
                    now_ms
                ],
            )?;
        }
        Ok(())
    }

    pub fn list_offers(&self) -> Result<Vec<StoredOffer>, DbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT d.device_id, p.name, o.space_id, o.name, o.mounts_json, o.members_json, o.received_at_ms
             FROM peer_offers o
             JOIN devices d ON d.ref = o.device_ref
             JOIN peers p ON p.device_ref = o.device_ref
             ORDER BY p.name, o.name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, [u8; 32]>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, [u8; 16]>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        let mut offers = Vec::new();
        for row in rows {
            let (id, peer_name, space_id, name, mounts_json, members_json, received_at_ms) = row?;
            let mounts: Vec<OfferedMount> = serde_json::from_str(&mounts_json)?;
            let members = parse_members_json(&members_json)?;
            offers.push(StoredOffer {
                peer: Device {
                    id: DeviceId::from_bytes(id),
                    name: peer_name,
                },
                space_id: space_from_bytes(space_id),
                name,
                mounts,
                members,
                received_at_ms,
            });
        }
        Ok(offers)
    }

    pub fn offer_from_peer(
        &self,
        peer: DeviceId,
        name_or_id: &str,
    ) -> Result<Option<StoredOffer>, DbError> {
        let offers = self.list_offers()?;
        Ok(offers.into_iter().find(|o| {
            o.peer.id == peer && (o.name == name_or_id || o.space_id.to_string() == name_or_id)
        }))
    }

    pub fn sync_progress(&self, peer: DeviceId, space: SpaceId) -> Result<SyncProgress, DbError> {
        let Some(device_ref) = self.device_ref(peer)? else {
            return Ok(SyncProgress {
                received_seq: Sequence::ZERO,
                acked_seq: Sequence::ZERO,
                last_sync_ms: None,
            });
        };
        let space_b = space_bytes(space);
        let row = self
            .conn
            .query_row(
                "SELECT received_seq, acked_seq, last_sync_ms
                 FROM sync_progress WHERE device_ref = ?1 AND space_id = ?2",
                params![device_ref, space_b.as_slice()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .optional()?;
        match row {
            None => Ok(SyncProgress {
                received_seq: Sequence::ZERO,
                acked_seq: Sequence::ZERO,
                last_sync_ms: None,
            }),
            Some((received, acked, last_sync_ms)) => Ok(SyncProgress {
                received_seq: Sequence(u64_from_i64(received)?),
                acked_seq: Sequence(u64_from_i64(acked)?),
                last_sync_ms,
            }),
        }
    }

    pub fn set_received_seq(
        &self,
        peer: DeviceId,
        space: SpaceId,
        seq: Sequence,
        now_ms: i64,
    ) -> Result<(), DbError> {
        self.upsert_progress(peer, space, Some(seq), None, Some(now_ms))
    }

    pub fn set_acked_seq(
        &self,
        peer: DeviceId,
        space: SpaceId,
        seq: Sequence,
        now_ms: i64,
    ) -> Result<(), DbError> {
        self.upsert_progress(peer, space, None, Some(seq), Some(now_ms))
    }

    pub fn replica_pushed_seq(&self, space: SpaceId) -> Result<Sequence, DbError> {
        let space = space_bytes(space);
        let seq: Option<i64> = self
            .conn
            .query_row(
                "SELECT pushed_seq FROM replica_push WHERE space_id = ?1",
                params![space.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        match seq {
            None => Ok(Sequence::ZERO),
            Some(v) => Ok(Sequence(u64_from_i64(v)?)),
        }
    }

    pub fn set_replica_pushed_seq(&self, space: SpaceId, seq: Sequence) -> Result<(), DbError> {
        let space = space_bytes(space);
        let pushed = i64_from_u64(seq.0)?;
        self.conn.execute(
            "INSERT INTO replica_push (space_id, pushed_seq) VALUES (?1, ?2)
             ON CONFLICT(space_id) DO UPDATE SET pushed_seq = excluded.pushed_seq",
            params![space.as_slice(), pushed],
        )?;
        Ok(())
    }

    pub fn peers_sharing_space(&self, space: SpaceId) -> Result<Vec<DeviceId>, DbError> {
        let space = space_bytes(space);
        let mut stmt = self.conn.prepare_cached(
            "SELECT d.device_id
             FROM space_shares s
             JOIN devices d ON d.ref = s.device_ref
             WHERE s.space_id = ?1
             ORDER BY d.device_id",
        )?;
        let rows = stmt.query_map(params![space.as_slice()], |row| row.get::<_, [u8; 32]>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(DeviceId::from_bytes(row?));
        }
        Ok(out)
    }

    pub fn reset_received_seq_for_space(&self, space: SpaceId) -> Result<(), DbError> {
        let space = space_bytes(space);
        self.conn.execute(
            "UPDATE sync_progress SET received_seq = 0 WHERE space_id = ?1",
            params![space.as_slice()],
        )?;
        Ok(())
    }

    pub fn list_progress_for_peer(
        &self,
        peer: DeviceId,
    ) -> Result<Vec<(SpaceId, SyncProgress)>, DbError> {
        let Some(device_ref) = self.device_ref(peer)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT space_id, received_seq, acked_seq, last_sync_ms
             FROM sync_progress WHERE device_ref = ?1",
        )?;
        let rows = stmt.query_map(params![device_ref], |row| {
            Ok((
                row.get::<_, [u8; 16]>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (space, received, acked, last_sync_ms) = row?;
            out.push((
                space_from_bytes(space),
                SyncProgress {
                    received_seq: Sequence(u64_from_i64(received)?),
                    acked_seq: Sequence(u64_from_i64(acked)?),
                    last_sync_ms,
                },
            ));
        }
        Ok(out)
    }

    pub fn list_delete_holds(&self) -> Result<Vec<DeleteHoldRow>, DbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT d.device_id, p.name,
                    s.id, s.name,
                    m.id, m.name,
                    h.deletions, h.live, h.held_at_ms, h.decision, h.decided_at_ms
             FROM delete_holds h
             JOIN devices d ON d.ref = h.device_ref
             JOIN peers p ON p.device_ref = h.device_ref
             JOIN spaces s ON s.id = h.space_id
             JOIN mounts m ON m.id = h.mount_id
             ORDER BY p.name, s.name, m.name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, [u8; 32]>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, [u8; 16]>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, [u8; 16]>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<i64>>(10)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (
                peer_id,
                peer_name,
                space_id,
                space_name,
                mount_id,
                mount_name,
                deletions,
                live,
                held_at_ms,
                decision,
                decided_at_ms,
            ) = row?;
            out.push(DeleteHoldRow {
                peer: Device {
                    id: DeviceId::from_bytes(peer_id),
                    name: peer_name,
                },
                space: Space {
                    id: space_from_bytes(space_id),
                    name: space_name,
                },
                mount: Mount {
                    id: mount_from_bytes(mount_id),
                    space: space_from_bytes(space_id),
                    name: mount_name,
                },
                deletions: usize::try_from(deletions).map_err(|_| DbError::IntegerOverflow)?,
                live: usize::try_from(live).map_err(|_| DbError::IntegerOverflow)?,
                held_at_ms,
                decision: decision.as_deref().map(parse_decision).transpose()?,
                decided_at_ms,
            });
        }
        Ok(out)
    }

    pub fn delete_hold(
        &self,
        peer: DeviceId,
        space: SpaceId,
        mount: MountId,
    ) -> Result<Option<DeleteHoldRow>, DbError> {
        Ok(self
            .list_delete_holds()?
            .into_iter()
            .find(|h| h.peer.id == peer && h.space.id == space && h.mount.id == mount))
    }

    pub fn upsert_delete_hold(
        &self,
        peer: DeviceId,
        space: SpaceId,
        mount: MountId,
        deletions: usize,
        live: usize,
        held_at_ms: i64,
    ) -> Result<(), DbError> {
        let device_ref = self.device_ref(peer)?.ok_or(DbError::NotFound)?;
        let space = space_bytes(space);
        let mount = mount_bytes(mount);
        let deletions = i64::try_from(deletions).map_err(|_| DbError::IntegerOverflow)?;
        let live = i64::try_from(live).map_err(|_| DbError::IntegerOverflow)?;
        self.conn.execute(
            "INSERT INTO delete_holds (
                 device_ref, space_id, mount_id, deletions, live, held_at_ms, decision, decided_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL)
             ON CONFLICT(device_ref, space_id, mount_id) DO UPDATE SET
                deletions = excluded.deletions,
                live = excluded.live,
                held_at_ms = excluded.held_at_ms",
            params![
                device_ref,
                space.as_slice(),
                mount.as_slice(),
                deletions,
                live,
                held_at_ms
            ],
        )?;
        Ok(())
    }

    pub fn decide_delete_holds(
        &self,
        space: SpaceId,
        mount: Option<MountId>,
        peer: Option<DeviceId>,
        decision: DeleteHoldDecision,
        decided_at_ms: i64,
    ) -> Result<usize, DbError> {
        let space = space_bytes(space);
        let mount = mount.map(mount_bytes);
        let peer_ref = match peer {
            Some(id) => Some(self.device_ref(id)?.ok_or(DbError::NotFound)?),
            None => None,
        };
        let changed = self.conn.execute(
            "UPDATE delete_holds
             SET decision = ?1, decided_at_ms = ?2
             WHERE space_id = ?3
               AND (?4 IS NULL OR mount_id = ?4)
               AND (?5 IS NULL OR device_ref = ?5)",
            params![
                decision.as_str(),
                decided_at_ms,
                space.as_slice(),
                mount.as_ref().map(|m| m.as_slice()),
                peer_ref
            ],
        )?;
        Ok(changed)
    }

    pub fn clear_delete_hold(
        &self,
        peer: DeviceId,
        space: SpaceId,
        mount: MountId,
    ) -> Result<(), DbError> {
        let Some(device_ref) = self.device_ref(peer)? else {
            return Ok(());
        };
        let space = space_bytes(space);
        let mount = mount_bytes(mount);
        self.conn.execute(
            "DELETE FROM delete_holds WHERE device_ref = ?1 AND space_id = ?2 AND mount_id = ?3",
            params![device_ref, space.as_slice(), mount.as_slice()],
        )?;
        Ok(())
    }

    pub fn clear_delete_holds_for_peer_space(
        &self,
        peer: DeviceId,
        space: SpaceId,
    ) -> Result<(), DbError> {
        let Some(device_ref) = self.device_ref(peer)? else {
            return Ok(());
        };
        let space = space_bytes(space);
        self.conn.execute(
            "DELETE FROM delete_holds WHERE device_ref = ?1 AND space_id = ?2",
            params![device_ref, space.as_slice()],
        )?;
        Ok(())
    }

    pub fn replace_delete_hold_paths(
        &self,
        peer: DeviceId,
        space: SpaceId,
        mount: MountId,
        paths: &[relay_core::LogicalPath],
    ) -> Result<(), DbError> {
        let device_ref = self.device_ref(peer)?.ok_or(DbError::NotFound)?;
        let space_b = space_bytes(space);
        let mount_b = mount_bytes(mount);
        self.conn.execute(
            "DELETE FROM delete_hold_paths
             WHERE device_ref = ?1 AND space_id = ?2 AND mount_id = ?3",
            params![device_ref, space_b.as_slice(), mount_b.as_slice()],
        )?;
        for path in paths {
            self.conn.execute(
                "INSERT OR IGNORE INTO delete_hold_paths
                     (device_ref, space_id, mount_id, path, applied)
                 VALUES (?1, ?2, ?3, ?4, 0)",
                params![
                    device_ref,
                    space_b.as_slice(),
                    mount_b.as_slice(),
                    path.as_str()
                ],
            )?;
        }
        Ok(())
    }

    pub fn insert_delete_hold_applied_paths(
        &self,
        peer: DeviceId,
        space: SpaceId,
        mount: MountId,
        paths: &[relay_core::LogicalPath],
    ) -> Result<(), DbError> {
        let device_ref = self.device_ref(peer)?.ok_or(DbError::NotFound)?;
        let space_b = space_bytes(space);
        let mount_b = mount_bytes(mount);
        for path in paths {
            self.conn.execute(
                "INSERT OR IGNORE INTO delete_hold_paths
                     (device_ref, space_id, mount_id, path, applied)
                 VALUES (?1, ?2, ?3, ?4, 1)",
                params![
                    device_ref,
                    space_b.as_slice(),
                    mount_b.as_slice(),
                    path.as_str()
                ],
            )?;
        }
        Ok(())
    }

    pub fn list_delete_hold_paths(
        &self,
        peer: DeviceId,
        space: SpaceId,
        mount: MountId,
    ) -> Result<Vec<relay_core::LogicalPath>, DbError> {
        self.list_delete_hold_paths_marked(peer, space, mount, Some(false))
    }

    pub fn list_delete_hold_applied_paths(
        &self,
        peer: DeviceId,
        space: SpaceId,
        mount: MountId,
    ) -> Result<Vec<relay_core::LogicalPath>, DbError> {
        self.list_delete_hold_paths_marked(peer, space, mount, Some(true))
    }

    fn list_delete_hold_paths_marked(
        &self,
        peer: DeviceId,
        space: SpaceId,
        mount: MountId,
        applied: Option<bool>,
    ) -> Result<Vec<relay_core::LogicalPath>, DbError> {
        let Some(device_ref) = self.device_ref(peer)? else {
            return Ok(Vec::new());
        };
        let space = space_bytes(space);
        let mount = mount_bytes(mount);
        let mut stmt = self.conn.prepare_cached(
            "SELECT path FROM delete_hold_paths
             WHERE device_ref = ?1 AND space_id = ?2 AND mount_id = ?3
               AND (?4 IS NULL OR applied = ?4)
             ORDER BY path",
        )?;
        let rows = stmt.query_map(
            params![
                device_ref,
                space.as_slice(),
                mount.as_slice(),
                applied.map(i64::from)
            ],
            |row| row.get::<_, String>(0),
        )?;
        let mut out = Vec::new();
        for row in rows {
            let raw = row?;
            out.push(
                relay_core::LogicalPath::new(&raw)
                    .map_err(|err| DbError::Corrupt(err.to_string()))?,
            );
        }
        Ok(out)
    }

    fn upsert_progress(
        &self,
        peer: DeviceId,
        space: SpaceId,
        received: Option<Sequence>,
        acked: Option<Sequence>,
        last_sync_ms: Option<i64>,
    ) -> Result<(), DbError> {
        let device_ref = self.device_ref(peer)?.ok_or(DbError::NotFound)?;
        let space = space_bytes(space);
        let received = received.map(|s| i64_from_u64(s.0)).transpose()?;
        let acked = acked.map(|s| i64_from_u64(s.0)).transpose()?;
        self.conn.execute(
            "INSERT INTO sync_progress (device_ref, space_id, received_seq, acked_seq, last_sync_ms)
             VALUES (?1, ?2, COALESCE(?3, 0), COALESCE(?4, 0), ?5)
             ON CONFLICT(device_ref, space_id) DO UPDATE SET
                received_seq = COALESCE(?3, sync_progress.received_seq),
                acked_seq = COALESCE(?4, sync_progress.acked_seq),
                last_sync_ms = COALESCE(?5, sync_progress.last_sync_ms)",
            params![
                device_ref,
                space.as_slice(),
                received,
                acked,
                last_sync_ms
            ],
        )?;
        Ok(())
    }

    fn load_peer(
        &self,
        where_clause: &str,
        params: impl rusqlite::Params,
    ) -> Result<Option<PeerRecord>, DbError> {
        let sql = format!(
            "SELECT d.device_id, p.name, p.addresses, p.added_at_ms, d.last_seen_ms
             FROM peers p
             JOIN devices d ON d.ref = p.device_ref
             WHERE {where_clause}"
        );
        self.conn
            .query_row(&sql, params, |row| {
                Ok((
                    row.get::<_, [u8; 32]>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })
            .optional()?
            .map(|(id, name, addresses, added_at_ms, last_seen_ms)| {
                parse_peer(id, name, addresses, added_at_ms, last_seen_ms)
            })
            .transpose()
    }

    pub fn create_materialization_rule(
        &self,
        id: MaterializationRuleId,
        space: SpaceId,
        name: &str,
        mode: &str,
        selectors: &[String],
        now_ms: i64,
    ) -> Result<(), DbError> {
        if !matches!(mode, "full" | "metadata" | "demand" | "exclude") {
            return Err(DbError::Corrupt(format!(
                "unknown materialization mode {mode:?}"
            )));
        }
        let id_bytes = materialization_rule_bytes(id);
        let space_bytes = space_bytes(space);
        let position: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1 FROM materialization_rules WHERE space_id = ?1",
            params![space_bytes.as_slice()],
            |row| row.get(0),
        )?;
        match self.conn.execute(
            "INSERT INTO materialization_rules (id, space_id, name, mode, position, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id_bytes.as_slice(),
                space_bytes.as_slice(),
                name,
                mode,
                position,
                now_ms
            ],
        ) {
            Ok(_) => {}
            Err(err) => return Err(map_write_err(err, Some(name))),
        }
        for (index, pattern) in selectors.iter().enumerate() {
            let selector_pos = i64::try_from(index).map_err(|_| DbError::IntegerOverflow)?;
            self.conn.execute(
                "INSERT INTO materialization_selectors (rule_id, position, pattern)
                 VALUES (?1, ?2, ?3)",
                params![id_bytes.as_slice(), selector_pos, pattern.as_str()],
            )?;
        }
        Ok(())
    }

    pub fn delete_materialization_rule(&self, space: SpaceId, name: &str) -> Result<(), DbError> {
        let space_bytes = space_bytes(space);
        let changed = self.conn.execute(
            "DELETE FROM materialization_rules WHERE space_id = ?1 AND name = ?2",
            params![space_bytes.as_slice(), name],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    /// Rules for `space`, ordered by `position` ascending (last match wins).
    pub fn list_materialization_rules(
        &self,
        space: SpaceId,
    ) -> Result<Vec<MaterializationRuleRecord>, DbError> {
        let space_bytes = space_bytes(space);
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, space_id, name, mode, position FROM materialization_rules
             WHERE space_id = ?1
             ORDER BY position",
        )?;
        let rows = stmt.query_map(params![space_bytes.as_slice()], |row| {
            Ok((
                row.get::<_, [u8; 16]>(0)?,
                row.get::<_, [u8; 16]>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, space_id, name, mode, position) = row?;
            if !matches!(mode.as_str(), "full" | "metadata" | "demand" | "exclude") {
                return Err(DbError::Corrupt(format!(
                    "unknown materialization mode {mode:?}"
                )));
            }
            let selectors = self.materialization_selectors(id)?;
            out.push(MaterializationRuleRecord {
                id: materialization_rule_from_bytes(id),
                space_id: space_from_bytes(space_id),
                name,
                mode,
                position,
                selectors,
            });
        }
        Ok(out)
    }

    fn materialization_selectors(&self, id: [u8; 16]) -> Result<Vec<String>, DbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT pattern FROM materialization_selectors
             WHERE rule_id = ?1
             ORDER BY position",
        )?;
        let rows = stmt.query_map(params![id.as_slice()], |row| row.get::<_, String>(0))?;
        let mut selectors = Vec::new();
        for row in rows {
            selectors.push(row?);
        }
        Ok(selectors)
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
                    modified_at_ms = ?10, stat_size = ?11, stat_mtime_ns = ?12, stat_file_id = ?13,
                    stat_ctime_ns = ?14, materialized = ?15
                 WHERE id = ?16",
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
                    stat.ctime_ns,
                    i64::from(record.materialized),
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
                    stat_size, stat_mtime_ns, stat_file_id, stat_ctime_ns, materialized
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
                    stat.ctime_ns,
                    i64::from(record.materialized),
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
            "INSERT OR IGNORE INTO history (
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
            stat: decode_stat(
                raw.stat_size,
                raw.stat_mtime_ns,
                raw.stat_file_id,
                raw.stat_ctime_ns,
            )?,
            materialized: raw.materialized != 0,
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
            stat_ctime_ns: row.get(16)?,
            space_id: row.get(17)?,
            materialized: row.get(18)?,
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

    fn local_device_ref(&self) -> Result<i64, DbError> {
        let local = self.local_device()?.ok_or(DbError::NotInitialized)?;
        self.device_ref(local.device.id)?
            .ok_or_else(|| DbError::Corrupt("local device row is missing".into()))
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
        let placeholder = id.short();
        self.conn.execute(
            "INSERT INTO devices (device_id, name, status, created_at_ms, last_seen_ms)
             VALUES (?1, ?3, 'active', ?2, ?2)",
            params![bytes.as_slice(), now_ms, placeholder],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn device_status(&self, id: DeviceId) -> Result<Option<String>, DbError> {
        let id = device_id_bytes(id);
        self.conn
            .query_row(
                "SELECT status FROM devices WHERE device_id = ?1",
                params![id.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(DbError::from)
    }

    pub fn set_device_status(&self, id: DeviceId, status: &str) -> Result<(), DbError> {
        let id = device_id_bytes(id);
        self.conn.execute(
            "UPDATE devices SET status = ?1 WHERE device_id = ?2",
            params![status, id.as_slice()],
        )?;
        Ok(())
    }

    pub fn put_peer_box_key(&self, id: DeviceId, public_key: &[u8; 32]) -> Result<(), DbError> {
        let id = device_id_bytes(id);
        self.conn.execute(
            "INSERT INTO peer_box_keys (device_id, public_key) VALUES (?1, ?2)
             ON CONFLICT(device_id) DO UPDATE SET public_key = excluded.public_key",
            params![id.as_slice(), public_key.as_slice()],
        )?;
        Ok(())
    }

    pub fn peer_box_key(&self, id: DeviceId) -> Result<Option<[u8; 32]>, DbError> {
        let id = device_id_bytes(id);
        self.conn
            .query_row(
                "SELECT public_key FROM peer_box_keys WHERE device_id = ?1",
                params![id.as_slice()],
                |row| row.get::<_, [u8; 32]>(0),
            )
            .optional()
            .map_err(DbError::from)
    }

    pub fn put_space_key_wrap(&self, wrap: &SpaceKeyWrap) -> Result<(), DbError> {
        let space = space_bytes(wrap.space);
        let recipient = device_id_bytes(wrap.recipient);
        let generation = i64::from(wrap.generation);
        self.conn.execute(
            "INSERT INTO space_key_wraps (space_id, generation, purpose, recipient, wrapped)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(space_id, generation, purpose, recipient)
             DO UPDATE SET wrapped = excluded.wrapped",
            params![
                space.as_slice(),
                generation,
                wrap.purpose,
                recipient.as_slice(),
                wrap.wrapped,
            ],
        )?;
        Ok(())
    }

    pub fn space_key_wraps(&self, space: SpaceId) -> Result<Vec<SpaceKeyWrap>, DbError> {
        let space_b = space_bytes(space);
        let mut stmt = self.conn.prepare_cached(
            "SELECT generation, purpose, recipient, wrapped
             FROM space_key_wraps WHERE space_id = ?1 ORDER BY generation",
        )?;
        let rows = stmt.query_map(params![space_b.as_slice()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, [u8; 32]>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (generation, purpose, recipient, wrapped) = row?;
            let generation = u32::try_from(generation).map_err(|_| DbError::IntegerOverflow)?;
            out.push(SpaceKeyWrap {
                space,
                generation,
                purpose,
                recipient: DeviceId::from_bytes(recipient),
                wrapped,
            });
        }
        Ok(out)
    }
}

/// One wrapped copy of a space key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceKeyWrap {
    pub space: SpaceId,
    pub generation: u32,
    pub purpose: String,
    pub recipient: DeviceId,
    pub wrapped: Vec<u8>,
}

fn parse_peer(
    id: [u8; 32],
    name: String,
    addresses: String,
    added_at_ms: i64,
    last_seen_ms: Option<i64>,
) -> Result<PeerRecord, DbError> {
    Ok(PeerRecord {
        device: Device {
            id: DeviceId::from_bytes(id),
            name,
        },
        addresses: serde_json::from_str(&addresses)?,
        added_at_ms,
        last_seen_ms,
    })
}

fn parse_members_json(raw: &str) -> Result<Vec<OfferedMember>, DbError> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(raw)?)
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
