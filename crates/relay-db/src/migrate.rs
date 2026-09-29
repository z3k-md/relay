use rusqlite::Connection;

use crate::DbError;

const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql")];

pub(crate) const SCHEMA_VERSION: u32 = 1;

pub(crate) fn user_version(conn: &Connection) -> Result<u32, DbError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    u32::try_from(version)
        .map_err(|_| DbError::Corrupt(format!("PRAGMA user_version out of range: {version}")))
}

pub(crate) fn migrate(conn: &mut Connection) -> Result<(), DbError> {
    let current = user_version(conn)?;
    if current > SCHEMA_VERSION {
        return Err(DbError::SchemaTooNew {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = u32::try_from(index + 1)
            .map_err(|_| DbError::Corrupt("migration index overflow".into()))?;
        if current >= version {
            continue;
        }
        tracing::debug!(version, "applying sqlite migration");
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}
