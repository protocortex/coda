// SPDX-License-Identifier: AGPL-3.0-only

//! Ordered, versioned schema migrations tracked by `PRAGMA user_version`.

use rusqlite::{Connection, TransactionBehavior};

use crate::{SCHEMA_VERSION, StoreError};

/// Every migration as `(version, sql)`, in ascending version order.
const MIGRATIONS: &[(u32, &str)] = &[(1, include_str!("schema.sql"))];

/// Brings `conn` up to [`SCHEMA_VERSION`], applying each pending migration in
/// its own transaction. Running it on an up-to-date database changes nothing.
///
/// # Errors
///
/// Returns [`StoreError::SchemaTooNew`] when the database was written by a
/// newer binary, and [`StoreError::Sqlite`] when a migration fails.
pub fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    for (version, sql) in MIGRATIONS {
        // IMMEDIATE takes the write lock before the version is read, so a
        // racing opener waits and then sees the migration already applied.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found = read_version(&tx)?;
        ensure_supported(found)?;
        if found >= *version {
            continue;
        }
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

/// Reads `PRAGMA user_version`.
pub(crate) fn read_version(conn: &Connection) -> Result<u32, StoreError> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Refuses a database written by a newer binary.
pub(crate) fn ensure_supported(found: u32) -> Result<(), StoreError> {
    if found > SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}
