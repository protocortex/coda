// SPDX-License-Identifier: AGPL-3.0-only

//! Standing permission grants, keyed by repository, tool, cwd and exact pattern.

use rusqlite::{OptionalExtension, params};

use crate::{Store, StoreError};

/// What identifies a grant. Every field must match exactly: the cwd is part
/// of the key because the same command can mean something else elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantKey<'a> {
    pub repo_path: &'a str,
    pub tool: &'a str,
    pub cwd: &'a str,
    pub pattern: &'a str,
}

/// A pattern allowed for a tool in a repository and cwd without asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub repo_path: String,
    pub tool: String,
    pub cwd: String,
    pub command_pattern: String,
    /// Grant time in milliseconds since the Unix epoch.
    pub granted_at: i64,
}

impl Store {
    /// Saves a grant. Saving one that exists already changes nothing, so the
    /// original grant time stays.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the insert fails.
    pub fn save_grant(&mut self, key: &GrantKey<'_>) -> Result<(), StoreError> {
        self.connection().execute(
            "INSERT OR IGNORE INTO permission_grants \
             (repo_path, tool, cwd, command_pattern, granted_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                key.repo_path,
                key.tool,
                key.cwd,
                key.pattern,
                self.clock.now_ms()
            ],
        )?;
        Ok(())
    }

    /// Finds the grant that equals `key` on every field.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the query fails.
    pub fn lookup_grant(&self, key: &GrantKey<'_>) -> Result<Option<Grant>, StoreError> {
        Ok(self
            .connection()
            .query_row(
                "SELECT repo_path, tool, cwd, command_pattern, granted_at \
                 FROM permission_grants \
                 WHERE repo_path = ?1 AND tool = ?2 AND cwd = ?3 AND command_pattern = ?4",
                [key.repo_path, key.tool, key.cwd, key.pattern],
                |row| {
                    Ok(Grant {
                        repo_path: row.get(0)?,
                        tool: row.get(1)?,
                        cwd: row.get(2)?,
                        command_pattern: row.get(3)?,
                        granted_at: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }
}
