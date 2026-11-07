// SPDX-License-Identifier: Apache-2.0

//! The backend's own conversation id per session, kept across daemon restarts.

use coda_protocol::types::{BackendKind, SessionId};
use rusqlite::{OptionalExtension, params};

use crate::sessions::{encode_enum, session_row_id};
use crate::{Store, StoreError};

const STATUS_ACTIVE: &str = "active";

impl Store {
    /// Records that `backend` reported `native_ref` as the conversation of
    /// `session`. A different ref replaces the one stored before.
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
    ) -> Result<(), StoreError> {
        let Some(row) = session_row_id(session) else {
            return Err(StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows));
        };
        self.connection().execute(
            "INSERT INTO backend_threads \
                 (session_id, backend, native_ref, status) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (session_id, backend) DO UPDATE SET \
                 native_ref = excluded.native_ref, \
                 status = excluded.status",
            params![row, encode_enum(&backend)?, native_ref, STATUS_ACTIVE],
        )?;
        Ok(())
    }

    /// Forgets the ref of `session` on `backend`, so its next turn starts a new
    /// conversation. A ref that was never recorded is not an error.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the write fails.
    pub fn forget_backend_thread(
        &mut self,
        session: &SessionId,
        backend: BackendKind,
    ) -> Result<(), StoreError> {
        let Some(row) = session_row_id(session) else {
            return Ok(());
        };
        self.connection().execute(
            "DELETE FROM backend_threads WHERE session_id = ?1 AND backend = ?2",
            params![row, encode_enum(&backend)?],
        )?;
        Ok(())
    }

    /// The ref to resume for `session` on `backend`, or `None` when none was
    /// recorded.
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
}
