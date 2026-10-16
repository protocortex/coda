// SPDX-License-Identifier: AGPL-3.0-only

//! Permission requests waiting for an answer.

use coda_protocol::types::{PermissionDecision, RequestId, SessionId};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::decisions::insert_decision;
use crate::sessions::session_id;
use crate::{Store, StoreError};

/// A permission request to queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPending {
    pub session: SessionId,
    pub repo_path: String,
    pub command: String,
}

/// A queued permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermission {
    pub id: RequestId,
    pub session: SessionId,
    pub repo_path: String,
    pub command: String,
    /// Request time in milliseconds since the Unix epoch.
    pub requested_at: i64,
}

/// Parses a request id, accepting only the canonical decimal form this store
/// issues, so "03" or "+3" never alias request 3.
fn request_row_id(id: &RequestId) -> Option<i64> {
    let row: i64 = id.as_str().parse().ok()?;
    (row.to_string() == id.as_str()).then_some(row)
}

impl Store {
    /// Queues a permission request.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] when the session does not exist.
    pub fn add_pending(&mut self, new: &NewPending) -> Result<RequestId, StoreError> {
        let session_row = self.existing_session_row(&new.session)?;
        self.connection().execute(
            "INSERT INTO pending_permissions (session_id, repo_path, command, requested_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![session_row, new.repo_path, new.command, self.clock.now_ms()],
        )?;
        Ok(RequestId::new(
            self.connection().last_insert_rowid().to_string(),
        ))
    }

    /// Marks a request answered and records the answer, atomically.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::PendingNotFound`] for an unknown request and
    /// [`StoreError::AlreadyResolved`] when it was answered before.
    pub fn resolve_pending(
        &mut self,
        id: &RequestId,
        decision: PermissionDecision,
    ) -> Result<(), StoreError> {
        let not_found = || StoreError::PendingNotFound {
            request: id.clone(),
        };
        let request_row = request_row_id(id).ok_or_else(not_found)?;
        let now = self.clock.now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (session_row, repo_path, command, resolved_at): (i64, String, String, Option<i64>) = tx
            .query_row(
                "SELECT session_id, repo_path, command, resolved_at \
                 FROM pending_permissions WHERE id = ?1",
                [request_row],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or_else(not_found)?;
        if resolved_at.is_some() {
            return Err(StoreError::AlreadyResolved {
                request: id.clone(),
            });
        }
        tx.execute(
            "UPDATE pending_permissions SET resolved_at = ?1 WHERE id = ?2",
            params![now, request_row],
        )?;
        insert_decision(&tx, session_row, &repo_path, &command, decision, now)?;
        tx.commit()?;
        Ok(())
    }

    /// Lists the unanswered requests of a session, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] when the session does not exist.
    pub fn list_pending(&self, session: &SessionId) -> Result<Vec<PendingPermission>, StoreError> {
        let session_row = self.existing_session_row(session)?;
        let mut stmt = self.connection().prepare(
            "SELECT id, repo_path, command, requested_at FROM pending_permissions \
             WHERE session_id = ?1 AND resolved_at IS NULL ORDER BY id",
        )?;
        let rows = stmt.query_map([session_row], |row| {
            Ok(PendingPermission {
                id: RequestId::new(row.get::<_, i64>(0)?.to_string()),
                session: session_id(session_row),
                repo_path: row.get(1)?,
                command: row.get(2)?,
                requested_at: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
