// SPDX-License-Identifier: Apache-2.0

//! Forked sessions: a new session that remembers where it came from.

use coda_protocol::types::{SessionId, TurnId};
use rusqlite::{OptionalExtension, params};

use crate::events::owned_turn_row;
use crate::sessions::{encode_enum, session_id, session_row_id};
use crate::{Store, StoreError};

/// The session and turn a forked session came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFork {
    /// The session that was forked.
    pub parent: SessionId,
    /// The last turn of the parent that the fork keeps.
    pub source_turn: TurnId,
}

impl Store {
    /// Makes a session with the backend, model, folder and mode of `parent` that
    /// records `source_turn` of `parent` as its fork point, and returns its id.
    /// Nothing is made when the turn is not one of the parent's.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] for a missing parent,
    /// [`StoreError::TurnNotFound`] when `source_turn` is not a turn of `parent`,
    /// and [`StoreError::Sqlite`] when a write fails.
    pub fn create_fork(
        &mut self,
        parent: &SessionId,
        source_turn: &TurnId,
    ) -> Result<SessionId, StoreError> {
        let record = self.load_session(parent)?;
        let parent_row = self.existing_session_row(parent)?;
        let turn_row = owned_turn_row(&self.conn, parent, parent_row, source_turn)?;
        let now = self.clock.now_ms();
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions (created_at, backend, model, cwd, mode) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                now,
                encode_enum(&record.backend)?,
                record.model,
                record.cwd,
                encode_enum(&record.mode)?,
            ],
        )?;
        let fork_row = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO session_forks (session_id, parent_session_id, source_turn) \
             VALUES (?1, ?2, ?3)",
            params![fork_row, parent_row, turn_row],
        )?;
        tx.commit()?;
        Ok(session_id(fork_row))
    }

    /// Where `session` was forked from; `None` for a session that is not a fork.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the read fails.
    pub fn fork_of(&self, session: &SessionId) -> Result<Option<SessionFork>, StoreError> {
        let Some(row) = session_row_id(session) else {
            return Ok(None);
        };
        Ok(self
            .conn
            .query_row(
                "SELECT parent_session_id, source_turn FROM session_forks WHERE session_id = ?1",
                [row],
                |r| {
                    Ok(SessionFork {
                        parent: session_id(r.get(0)?),
                        source_turn: TurnId::new(r.get::<_, i64>(1)?.to_string()),
                    })
                },
            )
            .optional()?)
    }
}
