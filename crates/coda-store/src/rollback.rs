// SPDX-License-Identifier: Apache-2.0

//! Turns a rewind undid: kept in the store, left out of the conversation.

use std::collections::HashSet;

use coda_protocol::types::{SessionId, TurnId};
use rusqlite::params;

use crate::events::owned_turn_row;
use crate::{Store, StoreError};

impl Store {
    /// Marks every turn of `session` after `turn` as rolled back, or every turn
    /// when `turn` is `None`, and returns how many were newly marked. A turn
    /// already marked keeps its first mark.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] or [`StoreError::TurnNotFound`]
    /// when `turn` is not part of the session.
    pub fn roll_back_turns_after(
        &mut self,
        session: &SessionId,
        turn: Option<&TurnId>,
    ) -> Result<u64, StoreError> {
        let session_row = self.existing_session_row(session)?;
        let after = match turn {
            Some(turn) => owned_turn_row(&self.conn, session, session_row, turn)?,
            None => 0,
        };
        let marked = self.conn.execute(
            "UPDATE turns SET rolled_back_at = ?1 \
             WHERE session_id = ?2 AND id > ?3 AND rolled_back_at IS NULL",
            params![self.clock.now_ms(), session_row, after],
        )?;
        Ok(marked as u64)
    }

    /// The turns of `session` a rewind rolled back.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] or [`StoreError::Sqlite`].
    pub fn rolled_back_turns(&self, session: &SessionId) -> Result<HashSet<TurnId>, StoreError> {
        let session_row = self.existing_session_row(session)?;
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM turns WHERE session_id = ?1 AND rolled_back_at IS NOT NULL")?;
        let rows = stmt.query_map([session_row], |row| {
            Ok(TurnId::new(row.get::<_, i64>(0)?.to_string()))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
