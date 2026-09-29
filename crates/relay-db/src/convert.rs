use relay_core::{DeviceId, EntryContent, MountId, ObjectId, SpaceId, StatHint};
use uuid::Uuid;

use crate::DbError;

pub(crate) fn i64_from_u64(value: u64) -> Result<i64, DbError> {
    i64::try_from(value).map_err(|_| DbError::IntegerOverflow)
}

pub(crate) fn u64_from_i64(value: i64) -> Result<u64, DbError> {
    u64::try_from(value).map_err(|_| DbError::IntegerOverflow)
}

pub(crate) fn space_bytes(id: SpaceId) -> [u8; 16] {
    *id.as_uuid().as_bytes()
}

pub(crate) fn mount_bytes(id: MountId) -> [u8; 16] {
    *id.as_uuid().as_bytes()
}

pub(crate) fn space_from_bytes(bytes: [u8; 16]) -> SpaceId {
    SpaceId::from_uuid(Uuid::from_bytes(bytes))
}

pub(crate) fn mount_from_bytes(bytes: [u8; 16]) -> MountId {
    MountId::from_uuid(Uuid::from_bytes(bytes))
}

pub(crate) fn object_id_from_blob(bytes: &[u8]) -> Result<ObjectId, DbError> {
    let arr: [u8; 32] = bytes.try_into().map_err(|_| {
        DbError::Corrupt(format!("object id has {} bytes, expected 32", bytes.len()))
    })?;
    Ok(ObjectId::from_bytes(arr))
}

pub(crate) fn opt_object_id(bytes: Option<Vec<u8>>) -> Result<Option<ObjectId>, DbError> {
    bytes.as_deref().map(object_id_from_blob).transpose()
}

pub(crate) struct EncodedContent {
    pub kind: Option<&'static str>,
    pub deleted: i64,
    pub object_id: Option<ObjectId>,
    pub size: Option<u64>,
    pub executable: i64,
    pub symlink_target: Option<String>,
}

pub(crate) fn encode_content(content: &EntryContent) -> EncodedContent {
    match content {
        EntryContent::File {
            object,
            size,
            executable,
        } => EncodedContent {
            kind: Some("file"),
            deleted: 0,
            object_id: Some(*object),
            size: Some(*size),
            executable: i64::from(*executable),
            symlink_target: None,
        },
        EntryContent::Directory => EncodedContent {
            kind: Some("directory"),
            deleted: 0,
            object_id: None,
            size: None,
            executable: 0,
            symlink_target: None,
        },
        EntryContent::Symlink { target } => EncodedContent {
            kind: Some("symlink"),
            deleted: 0,
            object_id: None,
            size: None,
            executable: 0,
            symlink_target: Some(target.clone()),
        },
        EntryContent::Deleted => EncodedContent {
            kind: None,
            deleted: 1,
            object_id: None,
            size: None,
            executable: 0,
            symlink_target: None,
        },
    }
}

pub(crate) fn decode_content(
    kind: Option<&str>,
    deleted: i64,
    object_id: Option<Vec<u8>>,
    size: Option<i64>,
    executable: i64,
    symlink_target: Option<&str>,
) -> Result<EntryContent, DbError> {
    let object = opt_object_id(object_id)?;
    let size = size.map(u64_from_i64).transpose()?;
    let executable = executable != 0;
    match (deleted != 0, kind, object, size, symlink_target) {
        (true, None, _, _, _) => Ok(EntryContent::Deleted),
        (false, Some("directory"), None, None, None) => Ok(EntryContent::Directory),
        (false, Some("file"), Some(object), Some(size), None) => Ok(EntryContent::File {
            object,
            size,
            executable,
        }),
        (false, Some("symlink"), None, None, Some(target)) => Ok(EntryContent::Symlink {
            target: target.to_owned(),
        }),
        _ => Err(DbError::Corrupt(format!(
            "entry content does not match a known shape (deleted={deleted}, kind={kind:?})"
        ))),
    }
}

pub(crate) struct EncodedStat {
    pub size: Option<i64>,
    pub mtime_ns: Option<i64>,
    pub file_id: Option<i64>,
    pub ctime_ns: Option<i64>,
}

pub(crate) fn encode_stat(stat: Option<StatHint>) -> Result<EncodedStat, DbError> {
    match stat {
        None => Ok(EncodedStat {
            size: None,
            mtime_ns: None,
            file_id: None,
            ctime_ns: None,
        }),
        Some(hint) => Ok(EncodedStat {
            size: Some(i64_from_u64(hint.size)?),
            mtime_ns: Some(hint.mtime_ns),
            file_id: hint.file_id.map(i64_from_u64).transpose()?,
            ctime_ns: hint.ctime_ns,
        }),
    }
}

pub(crate) fn decode_stat(
    size: Option<i64>,
    mtime_ns: Option<i64>,
    file_id: Option<i64>,
    ctime_ns: Option<i64>,
) -> Result<Option<StatHint>, DbError> {
    match (size, mtime_ns, file_id, ctime_ns) {
        (None, None, None, None) => Ok(None),
        (Some(size), Some(mtime_ns), file_id, ctime_ns) => Ok(Some(StatHint {
            size: u64_from_i64(size)?,
            mtime_ns,
            file_id: file_id.map(u64_from_i64).transpose()?,
            ctime_ns,
        })),
        _ => Err(DbError::Corrupt(
            "stat hint columns are partially populated".into(),
        )),
    }
}

pub(crate) fn is_unique_violation(err: &rusqlite::Error) -> bool {
    match err {
        rusqlite::Error::SqliteFailure(info, _) => {
            info.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || info.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
        }
        _ => false,
    }
}

pub(crate) fn is_fk_violation(err: &rusqlite::Error) -> bool {
    match err {
        rusqlite::Error::SqliteFailure(info, _) => {
            info.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY
        }
        _ => false,
    }
}

pub(crate) fn map_write_err(err: rusqlite::Error, duplicate_name: Option<&str>) -> DbError {
    if is_unique_violation(&err)
        && let Some(name) = duplicate_name
    {
        return DbError::DuplicateName(name.to_owned());
    }
    if is_fk_violation(&err) {
        return DbError::NotFound;
    }
    DbError::Sqlite(err)
}

pub(crate) fn device_id_bytes(id: DeviceId) -> [u8; 32] {
    *id.as_bytes()
}
