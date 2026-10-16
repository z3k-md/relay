use rusqlite::{Connection, TransactionBehavior};

use crate::DbError;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_mount_state.sql"),
    include_str!("../migrations/0003_stat_ctime_and_history_unique.sql"),
    include_str!("../migrations/0004_peers_and_sync.sql"),
    include_str!("../migrations/0005_delete_holds.sql"),
    include_str!("../migrations/0006_delete_hold_applied.sql"),
    include_str!("../migrations/0007_local_settings.sql"),
    include_str!("../migrations/0008_space_members.sql"),
    include_str!("../migrations/0009_replication_policies.sql"),
    include_str!("../migrations/0010_replica_push.sql"),
    include_str!("../migrations/0011_space_keys.sql"),
    include_str!("../migrations/0012_materialization.sql"),
    include_str!("../migrations/0013_peer_manage.sql"),
    include_str!("../migrations/0014_store_mode.sql"),
];

/// The schema this build writes. A database at a higher version is refused.
pub const SCHEMA_VERSION: u32 = 14;

pub(crate) fn user_version(conn: &Connection) -> Result<u32, DbError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    u32::try_from(version)
        .map_err(|_| DbError::Corrupt(format!("PRAGMA user_version out of range: {version}")))
}

pub(crate) fn migrate(conn: &mut Connection) -> Result<(), DbError> {
    if check_version(user_version(conn)?)? {
        return Ok(());
    }
    // Hold the write lock before deciding what to apply: another process
    // opening the same outdated database may have migrated it meanwhile.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = user_version(&tx)?;
    if check_version(current)? {
        return Ok(());
    }
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = u32::try_from(index + 1)
            .map_err(|_| DbError::Corrupt("migration index overflow".into()))?;
        if current >= version {
            continue;
        }
        tracing::debug!(version, "applying sqlite migration");
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
    }
    tx.commit()?;
    Ok(())
}

/// `Ok(true)` when `current` is already this build's schema.
fn check_version(current: u32) -> Result<bool, DbError> {
    if current > SCHEMA_VERSION {
        return Err(DbError::SchemaTooNew {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(current == SCHEMA_VERSION)
}
