// SPDX-License-Identifier: Apache-2.0

//! The pull requests a session opened.

use coda_protocol::types::SessionId;
use rusqlite::params;

use crate::sessions::session_row_id;
use crate::{Store, StoreError};

impl Store {
    /// Links a pull request address to a session. Linking the same address again changes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] when no such session exists.
    pub fn link_pull_request(&mut self, session: &SessionId, url: &str) -> Result<(), StoreError> {
        let not_found = || StoreError::SessionNotFound {
            session: session.clone(),
        };
        let row_id = session_row_id(session).ok_or_else(not_found)?;
        self.load_session(session)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO session_pull_requests (session_id, url, created_at) \
             VALUES (?1, ?2, ?3)",
            params![row_id, url, self.clock.now_ms()],
        )?;
        Ok(())
    }

    /// The pull requests linked to a session, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the query fails.
    pub fn session_pull_requests(&self, session: &SessionId) -> Result<Vec<String>, StoreError> {
        let Some(row_id) = session_row_id(session) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT url FROM session_pull_requests WHERE session_id = ?1 ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([row_id], |row| row.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
