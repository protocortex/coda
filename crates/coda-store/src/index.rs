// SPDX-License-Identifier: AGPL-3.0-only

//! Symbol index writers. The FTS5 table follows through triggers in the schema.

use rusqlite::params;

use crate::{Store, StoreError};

/// A symbol found in a file, as handed over by the indexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    /// One-based line of the definition.
    pub line: u32,
}

impl Store {
    /// Replaces every symbol stored for `file` in `repo` with `symbols`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when a write fails; the file keeps its
    /// previous symbols in that case.
    pub fn upsert_symbols(
        &mut self,
        repo: &str,
        file: &str,
        symbols: Vec<Symbol>,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM symbols WHERE repo_path = ?1 AND file_path = ?2",
            [repo, file],
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO symbols (repo_path, file_path, name, kind, line) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for symbol in symbols {
                insert.execute(params![repo, file, symbol.name, symbol.kind, symbol.line])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Removes every symbol stored for `file` in `repo`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the delete fails.
    pub fn clear_file(&mut self, repo: &str, file: &str) -> Result<(), StoreError> {
        self.connection().execute(
            "DELETE FROM symbols WHERE repo_path = ?1 AND file_path = ?2",
            [repo, file],
        )?;
        Ok(())
    }
}
