// SPDX-License-Identifier: Apache-2.0

//! The checkpoint refs kept on turn rows.

use coda_protocol::types::{SessionId, TurnId};
use rusqlite::params;

use crate::events::owned_turn_row;
use crate::{Store, StoreError};

/// The snapshots of one turn: `before` is set on the first turn of a session
/// only, `after` on every turn whose snapshot succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCheckpoint {
    pub turn: TurnId,
    pub before: Option<String>,
    pub after: Option<String>,
}

impl Store {
    /// Records the snapshot taken before `turn`, the first turn of its session.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] or [`StoreError::TurnNotFound`].
    pub fn record_checkpoint_before(
        &mut self,
        session: &SessionId,
        turn: &TurnId,
        reference: &str,
    ) -> Result<(), StoreError> {
        self.set_checkpoint(
            "UPDATE turns SET checkpoint_before = ?1 WHERE id = ?2",
            session,
            turn,
            reference,
        )
    }

    /// Records the snapshot taken when `turn` ended.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] or [`StoreError::TurnNotFound`].
    pub fn record_checkpoint_after(
        &mut self,
        session: &SessionId,
        turn: &TurnId,
        reference: &str,
    ) -> Result<(), StoreError> {
        self.set_checkpoint(
            "UPDATE turns SET checkpoint_after = ?1 WHERE id = ?2",
            session,
            turn,
            reference,
        )
    }

    /// Forgets the given refs on every turn row that names them, and returns how
    /// many rows changed. Used after the refs are removed from git, so a rewind
    /// to those turns says they have no snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when an update fails.
    pub fn clear_checkpoint_refs(&mut self, refs: &[String]) -> Result<usize, StoreError> {
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        for reference in refs {
            changed += tx.execute(
                "UPDATE turns SET checkpoint_before = NULL WHERE checkpoint_before = ?1",
                [reference],
            )?;
            changed += tx.execute(
                "UPDATE turns SET checkpoint_after = NULL WHERE checkpoint_after = ?1",
                [reference],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// The checkpoints of every turn of `session`, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] or [`StoreError::Sqlite`].
    pub fn turn_checkpoints(&self, session: &SessionId) -> Result<Vec<TurnCheckpoint>, StoreError> {
        let session_row = self.existing_session_row(session)?;
        let mut stmt = self.conn.prepare(
            "SELECT id, checkpoint_before, checkpoint_after FROM turns \
             WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([session_row], |row| {
            Ok(TurnCheckpoint {
                turn: TurnId::new(row.get::<_, i64>(0)?.to_string()),
                before: row.get(1)?,
                after: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn set_checkpoint(
        &mut self,
        sql: &str,
        session: &SessionId,
        turn: &TurnId,
        reference: &str,
    ) -> Result<(), StoreError> {
        let session_row = self.existing_session_row(session)?;
        let turn_row = owned_turn_row(&self.conn, session, session_row, turn)?;
        self.conn.execute(sql, params![reference, turn_row])?;
        Ok(())
    }
}
