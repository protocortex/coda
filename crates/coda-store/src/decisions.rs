// SPDX-License-Identifier: AGPL-3.0-only

//! The audit trail of permission answers.

use coda_protocol::types::{PermissionDecision, SessionId};
use rusqlite::{Connection, params};

use crate::{Store, StoreError};

/// A permission answer to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDecision {
    pub session: SessionId,
    pub repo_path: String,
    pub command: String,
    /// Name of the sub-agent that asked; `None` for the session's own call.
    pub agent: Option<String>,
    pub decision: PermissionDecision,
}

/// A recorded permission answer. The store keeps only allow or deny.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDecision {
    pub repo_path: String,
    pub command: String,
    /// Name of the sub-agent that asked; `None` for the session's own call.
    pub agent: Option<String>,
    pub allowed: bool,
    /// Decision time in milliseconds since the Unix epoch.
    pub decided_at: i64,
}

/// Anything other than a known allow is stored as a deny.
fn is_allow(decision: PermissionDecision) -> bool {
    matches!(
        decision,
        PermissionDecision::AllowOnce | PermissionDecision::AllowAlways
    )
}

pub(crate) fn insert_decision(
    conn: &Connection,
    session_row: i64,
    repo_path: &str,
    command: &str,
    agent: Option<&str>,
    decision: PermissionDecision,
    decided_at: i64,
) -> Result<(), StoreError> {
    let text = if is_allow(decision) { "allow" } else { "deny" };
    conn.execute(
        "INSERT INTO permission_decisions \
         (session_id, repo_path, command, agent, decision, decided_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![session_row, repo_path, command, agent, text, decided_at],
    )?;
    Ok(())
}

impl Store {
    /// Records a permission answer.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] when the session does not exist.
    pub fn record_decision(&mut self, new: &NewDecision) -> Result<(), StoreError> {
        let session_row = self.existing_session_row(&new.session)?;
        insert_decision(
            self.connection(),
            session_row,
            &new.repo_path,
            &new.command,
            new.agent.as_deref(),
            new.decision,
            self.clock.now_ms(),
        )
    }

    /// Lists the answers recorded for a session, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] when the session does not exist.
    pub fn list_decisions(&self, session: &SessionId) -> Result<Vec<StoredDecision>, StoreError> {
        let session_row = self.existing_session_row(session)?;
        let mut stmt = self.connection().prepare(
            "SELECT repo_path, command, agent, decision, decided_at FROM permission_decisions \
             WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([session_row], |row| {
            Ok(StoredDecision {
                repo_path: row.get(0)?,
                command: row.get(1)?,
                agent: row.get(2)?,
                allowed: row.get::<_, String>(3)? == "allow",
                decided_at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
