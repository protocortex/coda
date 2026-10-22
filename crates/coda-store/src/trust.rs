// SPDX-License-Identifier: AGPL-3.0-only

//! The per repository decision to trust project agents, skills and commands.

use rusqlite::{OptionalExtension, params};

use crate::{Store, StoreError};

impl Store {
    /// Records that the user trusts the project definitions of `repo_path` as
    /// they are now, summarised by `digest`. Trusting again replaces the digest.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the write fails.
    pub fn trust_project(&mut self, repo_path: &str, digest: &str) -> Result<(), StoreError> {
        self.connection().execute(
            "INSERT INTO project_trust (repo_path, digest, trusted_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (repo_path) DO UPDATE SET digest = ?2, trusted_at = ?3",
            params![repo_path, digest, self.clock.now_ms()],
        )?;
        Ok(())
    }

    /// Removes the trust decision of `repo_path`; true when there was one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the delete fails.
    pub fn revoke_project_trust(&mut self, repo_path: &str) -> Result<bool, StoreError> {
        let removed = self.connection().execute(
            "DELETE FROM project_trust WHERE repo_path = ?1",
            [repo_path],
        )?;
        Ok(removed > 0)
    }

    /// The digest of the project definitions the user trusted in `repo_path`,
    /// or `None` when they never decided.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the query fails.
    pub fn project_trust_digest(&self, repo_path: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .connection()
            .query_row(
                "SELECT digest FROM project_trust WHERE repo_path = ?1",
                [repo_path],
                |row| row.get(0),
            )
            .optional()?)
    }
}
