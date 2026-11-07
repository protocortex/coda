// SPDX-License-Identifier: Apache-2.0

//! Forked sessions: a new session that remembers where it came from.

use std::collections::HashSet;

use coda_protocol::types::{SessionId, TurnId};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

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
    /// The turns up to `source_turn` that the parent had rolled back when the fork
    /// was made; the fork leaves them out whatever the parent does later.
    pub rolled_back: HashSet<TurnId>,
}

const TURN_ID_SEPARATOR: char = ',';

impl Store {
    /// Makes a session with the backend, model, folder and mode of `parent` that
    /// records `source_turn` of `parent` as its fork point, and returns its id.
    /// Nothing is made when the turn is not one of the parent's or is rolled back.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] for a missing parent,
    /// [`StoreError::TurnNotFound`] when `source_turn` is not a turn of `parent`,
    /// [`StoreError::TurnRolledBack`] when it is rolled back, and
    /// [`StoreError::Sqlite`] when a write fails.
    pub fn create_fork(
        &mut self,
        parent: &SessionId,
        source_turn: &TurnId,
    ) -> Result<SessionId, StoreError> {
        let record = self.load_session(parent)?;
        let parent_row = self.existing_session_row(parent)?;
        let turn_row = owned_turn_row(&self.conn, parent, parent_row, source_turn)?;
        let now = self.clock.now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let source_is_rolled_back: bool = tx.query_row(
            "SELECT rolled_back_at IS NOT NULL FROM turns WHERE id = ?1",
            [turn_row],
            |r| r.get(0),
        )?;
        if source_is_rolled_back {
            return Err(StoreError::TurnRolledBack {
                session: parent.clone(),
                turn: source_turn.clone(),
            });
        }
        let rolled_back = {
            let mut stmt = tx.prepare(
                "SELECT id FROM turns WHERE session_id = ?1 AND id < ?2 \
                 AND rolled_back_at IS NOT NULL ORDER BY id",
            )?;
            let ids = stmt.query_map(params![parent_row, turn_row], |r| r.get::<_, i64>(0))?;
            ids.map(|id| id.map(|id| id.to_string()))
                .collect::<Result<Vec<_>, _>>()?
                .join(&TURN_ID_SEPARATOR.to_string())
        };
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
            "INSERT INTO session_forks \
                 (session_id, parent_session_id, source_turn, rolled_back) \
             VALUES (?1, ?2, ?3, ?4)",
            params![fork_row, parent_row, turn_row, rolled_back],
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
                "SELECT parent_session_id, source_turn, rolled_back \
                 FROM session_forks WHERE session_id = ?1",
                [row],
                |r| {
                    let rolled_back: String = r.get(2)?;
                    Ok(SessionFork {
                        parent: session_id(r.get(0)?),
                        source_turn: TurnId::new(r.get::<_, i64>(1)?.to_string()),
                        rolled_back: rolled_back
                            .split(TURN_ID_SEPARATOR)
                            .filter(|id| !id.is_empty())
                            .map(TurnId::new)
                            .collect(),
                    })
                },
            )
            .optional()?)
    }
}
