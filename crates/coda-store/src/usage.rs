// SPDX-License-Identifier: AGPL-3.0-only

//! Token usage per turn. Holds counts and timestamps only, never content.

use coda_protocol::types::{SessionId, TurnId};
use rusqlite::params;

use crate::events::owned_turn_row;
use crate::sessions::{get_u64, to_db_int};
use crate::{Store, StoreError};

/// Tokens spent by one model call within a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecord {
    pub session: SessionId,
    pub turn: TurnId,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Summed token counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageSummary {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Store {
    /// Records token usage for a turn.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] or [`StoreError::TurnNotFound`]
    /// when the session or turn does not exist, and
    /// [`StoreError::ValueOutOfRange`] for a count above `i64::MAX`.
    pub fn record_usage(&mut self, record: &UsageRecord) -> Result<(), StoreError> {
        let session_row = self.existing_session_row(&record.session)?;
        let turn_row = owned_turn_row(
            self.connection(),
            &record.session,
            session_row,
            &record.turn,
        )?;
        self.connection().execute(
            "INSERT INTO usage (turn_id, input_tokens, output_tokens, at) VALUES (?1, ?2, ?3, ?4)",
            params![
                turn_row,
                to_db_int(record.input_tokens)?,
                to_db_int(record.output_tokens)?,
                self.clock.now_ms()
            ],
        )?;
        Ok(())
    }

    /// Sums usage for one session, or for every session when `session` is `None`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] when a given session does not
    /// exist, and [`StoreError::Sqlite`] when a sum overflows `i64`.
    pub fn usage_summary(&self, session: Option<&SessionId>) -> Result<UsageSummary, StoreError> {
        let session_row = session.map(|s| self.existing_session_row(s)).transpose()?;
        Ok(self.connection().query_row(
            "SELECT COALESCE(SUM(u.input_tokens), 0), COALESCE(SUM(u.output_tokens), 0) \
             FROM usage u JOIN turns t ON t.id = u.turn_id \
             WHERE (?1 IS NULL OR t.session_id = ?1)",
            [session_row],
            |row| {
                Ok(UsageSummary {
                    input_tokens: get_u64(row, 0)?,
                    output_tokens: get_u64(row, 1)?,
                })
            },
        )?)
    }
}
