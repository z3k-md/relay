//! Durable local SQLite index for one Relay device.

mod convert;
mod error;
mod migrate;
mod repo;

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};

pub use error::DbError;
pub use repo::{HistoryRecord, LocalDevice, MountConfig, MountState, Repo};

#[derive(Debug)]
pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::configure(conn)
    }

    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        Self::configure(conn)
    }

    /// Open an existing database without running migrations or taking a write lock.
    ///
    /// The on-disk schema version must already match this binary.
    pub fn open_read_only(path: &Path) -> Result<Self, DbError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)?;
        Self::configure_read_only(conn)
    }

    pub fn schema_version(&self) -> Result<u32, DbError> {
        migrate::user_version(&self.conn)
    }

    pub fn repo(&self) -> Repo<'_> {
        Repo { conn: &self.conn }
    }

    /// Run `f` in one IMMEDIATE transaction; commit on Ok, roll back on Err.
    pub fn transaction<T, E: From<DbError>>(
        &mut self,
        f: impl FnOnce(&Repo<'_>) -> Result<T, E>,
    ) -> Result<T, E> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(DbError::from)?;
        let outcome = f(&Repo { conn: &tx });
        match outcome {
            Ok(value) => {
                tx.commit().map_err(DbError::from)?;
                Ok(value)
            }
            Err(err) => Err(err),
        }
    }

    fn configure(mut conn: Connection) -> Result<Self, DbError> {
        conn.busy_timeout(Duration::from_secs(5))?;
        let journal_mode: String =
            conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        tracing::debug!(journal_mode, "opened sqlite connection");
        conn.execute("PRAGMA foreign_keys = ON", [])?;
        conn.execute("PRAGMA synchronous = FULL", [])?;
        migrate::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    fn configure_read_only(conn: Connection) -> Result<Self, DbError> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute("PRAGMA foreign_keys = ON", [])?;
        let version = migrate::user_version(&conn)?;
        if version != migrate::SCHEMA_VERSION {
            return Err(if version > migrate::SCHEMA_VERSION {
                DbError::SchemaTooNew {
                    found: version,
                    supported: migrate::SCHEMA_VERSION,
                }
            } else {
                DbError::SchemaTooOld {
                    found: version,
                    supported: migrate::SCHEMA_VERSION,
                }
            });
        }
        Ok(Self { conn })
    }
}

#[cfg(test)]
mod tests;
