// SPDX-License-Identifier: Apache-2.0

//! The answers kept for commands that carried a client request id, so a repeat
//! of the request returns the answer instead of doing the work again.

use coda_protocol::types::{SessionId, TurnId};
use rusqlite::{OptionalExtension, params};

use crate::sessions::{SessionRecord, session_id, session_row_id};
use crate::{NewSession, Store, StoreError};

/// How long a receipt is kept, in milliseconds: seven days. A retry comes
/// within seconds, so this only bounds the table.
pub const RECEIPT_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// What a command with a request id answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// The method that was called.
    pub method: String,
    /// The session the command named, or made.
    pub session: Option<SessionId>,
    /// The answer, as the method returned it.
    pub result_json: String,
}

impl Store {
    /// The receipt of `request_id`, when a command with that id was done.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the query fails.
    pub fn receipt(&self, request_id: &str) -> Result<Option<Receipt>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT method, session_id, result_json FROM command_receipts \
                 WHERE request_id = ?1",
                [request_id],
                |row| {
                    Ok(Receipt {
                        method: row.get(0)?,
                        session: row.get::<_, Option<i64>>(1)?.map(session_id),
                        result_json: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Keeps `result_json` as the answer to `request_id`, and drops receipts
    /// older than [`RECEIPT_RETENTION_MS`]. Returns `false`, keeping the first
    /// answer, when the id already has one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the write fails.
    pub fn record_receipt(
        &mut self,
        request_id: &str,
        method: &str,
        session: Option<&SessionId>,
        result_json: &str,
    ) -> Result<bool, StoreError> {
        let now = self.clock.now_ms();
        self.conn.execute(
            "DELETE FROM command_receipts WHERE at < ?1",
            [now - RECEIPT_RETENTION_MS],
        )?;
        let inserted = self.conn.execute(
            "INSERT INTO command_receipts (request_id, session_id, method, result_json, at) \
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (request_id) DO NOTHING",
            params![
                request_id,
                session.and_then(session_row_id),
                method,
                result_json,
                now
            ],
        )?;
        Ok(inserted > 0)
    }

    /// Starts a turn in `session` and keeps its id as the answer to `request_id`,
    /// in one transaction: either both are stored or neither is.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::DuplicateRequest`] when the id already has an
    /// answer, [`StoreError::SessionNotFound`] when the session does not exist,
    /// and [`StoreError::Sqlite`] when a write fails.
    pub fn begin_turn_for_request(
        &mut self,
        session: &SessionId,
        request_id: &str,
        method: &str,
    ) -> Result<TurnId, StoreError> {
        self.in_transaction(|store| {
            let turn = store.begin_turn(session)?;
            let answer = serde_json::json!({ "turn_id": turn }).to_string();
            if store.record_receipt(request_id, method, Some(session), &answer)? {
                Ok(turn)
            } else {
                Err(StoreError::DuplicateRequest {
                    request_id: request_id.to_owned(),
                })
            }
        })
    }

    /// Makes a session and keeps `answer(record)` as the answer to `request_id`,
    /// in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::DuplicateRequest`] when the id already has an
    /// answer, and [`StoreError::Sqlite`] when a write fails.
    pub fn create_session_for_request(
        &mut self,
        new: &NewSession,
        request_id: &str,
        method: &str,
        answer: impl FnOnce(&SessionRecord) -> String,
    ) -> Result<SessionRecord, StoreError> {
        self.in_transaction(|store| {
            let id = store.create_session(new)?;
            let record = store.load_session(&id)?;
            if store.record_receipt(request_id, method, Some(&id), &answer(&record))? {
                Ok(record)
            } else {
                Err(StoreError::DuplicateRequest {
                    request_id: request_id.to_owned(),
                })
            }
        })
    }

    /// Drops the receipt of `request_id`, for a request whose work failed after
    /// the receipt was stored, so a retry runs the work again.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the delete fails.
    pub fn forget_receipt(&mut self, request_id: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM command_receipts WHERE request_id = ?1",
            [request_id],
        )?;
        Ok(())
    }

    /// Runs `work` between `BEGIN` and `COMMIT`, rolling back when it fails.
    fn in_transaction<T>(
        &mut self,
        work: impl FnOnce(&mut Self) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match work(self) {
            Ok(value) => match self.conn.execute_batch("COMMIT") {
                Ok(()) => Ok(value),
                Err(error) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    Err(error.into())
                }
            },
            Err(error) => {
                // The work's own error is the one to report.
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }
}
