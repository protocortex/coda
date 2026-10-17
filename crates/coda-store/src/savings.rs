// SPDX-License-Identifier: AGPL-3.0-only

//! Bytes the token economy kept out of the context. Holds counts only, never content.

use coda_protocol::types::SessionId;
use rusqlite::params;

use crate::sessions::{get_u64, to_db_int};
use crate::{Store, StoreError};

/// One shrunk tool output: its size before and after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavingRecord {
    pub session: SessionId,
    pub original_bytes: u64,
    pub compressed_bytes: u64,
}

impl Store {
    /// Records one saving.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SavingExceedsOriginal`] when the compressed size
    /// is larger than the original, [`StoreError::SessionNotFound`] when the
    /// session does not exist, and [`StoreError::ValueOutOfRange`] for a size
    /// above `i64::MAX`.
    pub fn record_saving(&mut self, record: &SavingRecord) -> Result<(), StoreError> {
        if record.compressed_bytes > record.original_bytes {
            return Err(StoreError::SavingExceedsOriginal {
                original: record.original_bytes,
                compressed: record.compressed_bytes,
            });
        }
        let session_row = self.existing_session_row(&record.session)?;
        self.connection().execute(
            "INSERT INTO savings (session_id, original_bytes, compressed_bytes, at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                session_row,
                to_db_int(record.original_bytes)?,
                to_db_int(record.compressed_bytes)?,
                self.clock.now_ms()
            ],
        )?;
        Ok(())
    }

    /// Bytes kept out of the context for one session, or for every session when `session` is `None`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] when a given session does not
    /// exist, and [`StoreError::Sqlite`] when the sum overflows `i64`.
    pub fn saved_bytes(&self, session: Option<&SessionId>) -> Result<u64, StoreError> {
        let session_row = session.map(|s| self.existing_session_row(s)).transpose()?;
        Ok(self.connection().query_row(
            "SELECT COALESCE(SUM(original_bytes - compressed_bytes), 0) FROM savings \
             WHERE (?1 IS NULL OR session_id = ?1)",
            [session_row],
            |row| get_u64(row, 0),
        )?)
    }
}
