// SPDX-License-Identifier: Apache-2.0

//! The backend's own conversation id per session, kept across daemon restarts.

use coda_protocol::types::{BackendKind, SessionId, TurnId};
use rusqlite::{OptionalExtension, params};

use crate::sessions::{encode_enum, session_row_id};
use crate::{Store, StoreError};

const STATUS_ACTIVE: &str = "active";
const STATUS_ERROR: &str = "error";

impl Store {
    /// Records that `backend` reported `native_ref` as the conversation of
    /// `session` during `turn`. The same ref again moves only the last turn; a
    /// different one replaces the row and starts over from `turn`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the write fails, including a
    /// `session` that does not exist.
    pub fn record_backend_thread(
        &mut self,
        session: &SessionId,
        backend: BackendKind,
        native_ref: &str,
        turn: &TurnId,
    ) -> Result<(), StoreError> {
        let Some(row) = session_row_id(session) else {
            return Err(StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows));
        };
        self.connection().execute(
            "INSERT INTO backend_threads \
                 (session_id, backend, native_ref, first_turn, last_turn, status) \
             VALUES (?1, ?2, ?3, ?4, ?4, ?5) \
             ON CONFLICT (session_id, backend) DO UPDATE SET \
                 first_turn = CASE WHEN native_ref = excluded.native_ref \
                                   THEN first_turn ELSE excluded.first_turn END, \
                 native_ref = excluded.native_ref, \
                 last_turn = excluded.last_turn, \
                 status = excluded.status",
            params![
                row,
                encode_enum(&backend)?,
                native_ref,
                turn.as_str(),
                STATUS_ACTIVE
            ],
        )?;
        Ok(())
    }

    /// The ref to resume for `session` on `backend`, or `None` when none was
    /// recorded or the last resume of it failed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the query fails.
    pub fn active_backend_ref(
        &self,
        session: &SessionId,
        backend: BackendKind,
    ) -> Result<Option<String>, StoreError> {
        let Some(row) = session_row_id(session) else {
            return Ok(None);
        };
        Ok(self
            .connection()
            .query_row(
                "SELECT native_ref FROM backend_threads \
                 WHERE session_id = ?1 AND backend = ?2 AND status = ?3",
                params![row, encode_enum(&backend)?, STATUS_ACTIVE],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Marks the ref of `session` on `backend` as one that failed to resume,
    /// so the next start does not try it again. Does nothing when none exists.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the write fails.
    pub fn fail_backend_thread(
        &mut self,
        session: &SessionId,
        backend: BackendKind,
    ) -> Result<(), StoreError> {
        let Some(row) = session_row_id(session) else {
            return Ok(());
        };
        self.connection().execute(
            "UPDATE backend_threads SET status = ?3 WHERE session_id = ?1 AND backend = ?2",
            params![row, encode_enum(&backend)?, STATUS_ERROR],
        )?;
        Ok(())
    }
}
