// SPDX-License-Identifier: AGPL-3.0-only

//! Full tool outputs, kept apart from the content-free log and scoped to a session.

use coda_protocol::types::{OriginalId, SessionId};
use rusqlite::{OptionalExtension, params};

use crate::{Store, StoreError};

/// Largest original the store accepts, in bytes.
pub const MAX_ORIGINAL_BYTES: usize = 4 * 1024 * 1024;

/// Most original bytes one session keeps; storing past it evicts the oldest first.
pub const MAX_SESSION_ORIGINAL_BYTES: usize = 16 * 1024 * 1024;

fn row_id(id: &OriginalId) -> Option<i64> {
    id.as_str().parse().ok()
}

fn decode(bytes: Vec<u8>) -> Result<String, StoreError> {
    String::from_utf8(bytes).map_err(|e| {
        StoreError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Blob,
            Box::new(e),
        ))
    })
}

impl Store {
    /// Stores `content` under a new id owned by `session`.
    ///
    /// When the session would pass [`MAX_SESSION_ORIGINAL_BYTES`], its oldest
    /// originals are emptied (the row stays as an eviction marker, since a
    /// stored original is never empty).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::OriginalTooLarge`] above [`MAX_ORIGINAL_BYTES`],
    /// [`StoreError::SessionNotFound`] when the session does not exist, and
    /// [`StoreError::Sqlite`] when the insert fails.
    pub fn store_original(
        &mut self,
        session: &SessionId,
        content: &str,
    ) -> Result<OriginalId, StoreError> {
        if content.len() > MAX_ORIGINAL_BYTES {
            return Err(StoreError::OriginalTooLarge {
                size: content.len(),
                max: MAX_ORIGINAL_BYTES,
            });
        }
        let session_row = self.existing_session_row(session)?;
        self.evict_for(session_row, content.len())?;
        self.connection().execute(
            "INSERT INTO originals (session_id, content, at) VALUES (?1, ?2, ?3)",
            params![session_row, content.as_bytes(), self.clock.now_ms()],
        )?;
        Ok(OriginalId::new(
            self.connection().last_insert_rowid().to_string(),
        ))
    }

    fn evict_for(&self, session_row: i64, incoming: usize) -> Result<(), StoreError> {
        let room = MAX_SESSION_ORIGINAL_BYTES - incoming;
        let mut stmt = self.connection().prepare(
            "SELECT id, length(content) FROM originals \
             WHERE session_id = ?1 AND length(content) > 0 ORDER BY id",
        )?;
        let live: Vec<(i64, i64)> = stmt
            .query_map([session_row], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut total: usize = live
            .iter()
            .map(|&(_, n)| usize::try_from(n).unwrap_or(0))
            .sum();
        for (id, len) in live {
            if total <= room {
                break;
            }
            self.connection()
                .execute("UPDATE originals SET content = x'' WHERE id = ?1", [id])?;
            total -= usize::try_from(len).unwrap_or(0);
        }
        Ok(())
    }

    /// Loads an original, or `None` when `id` is unknown or belongs to another session.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::OriginalEvicted`] when it was dropped to make
    /// room, [`StoreError::SessionNotFound`] when the session does not exist,
    /// and [`StoreError::Sqlite`] when the read fails.
    pub fn load_original(
        &self,
        session: &SessionId,
        id: &OriginalId,
    ) -> Result<Option<String>, StoreError> {
        let session_row = self.existing_session_row(session)?;
        let Some(original_row) = row_id(id) else {
            return Ok(None);
        };
        self.connection()
            .query_row(
                "SELECT content FROM originals WHERE id = ?1 AND session_id = ?2",
                [original_row, session_row],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(|bytes| {
                if bytes.is_empty() {
                    Err(StoreError::OriginalEvicted {
                        original: id.clone(),
                    })
                } else {
                    decode(bytes)
                }
            })
            .transpose()
    }
}
