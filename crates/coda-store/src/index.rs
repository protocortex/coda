// SPDX-License-Identifier: AGPL-3.0-only

//! Symbol index writers. The FTS5 table follows through triggers in the schema.

use rusqlite::{Transaction, params};

use crate::{Store, StoreError};

/// A symbol found in a file, as handed over by the indexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    /// One-based line of the definition.
    pub line: u32,
    /// One-based last line of the definition.
    pub end_line: u32,
    /// Name of the nearest enclosing definition, if any.
    pub parent: Option<String>,
}

/// What the index remembers about one file, for change detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFileState {
    pub file_path: String,
    pub content_hash: String,
    /// Milliseconds since the Unix epoch when the file was last indexed.
    pub indexed_at: i64,
}

/// How much of a repo is indexed and how fresh it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStatus {
    pub files: u64,
    pub symbols: u64,
    /// Latest per-file index time, or `None` when nothing is indexed.
    pub last_indexed_at: Option<i64>,
}

impl Store {
    /// Replaces the symbols of `file` in `repo` and records `content_hash` and
    /// the current time, all in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when a write fails; the file keeps its
    /// previous symbols and hash in that case.
    pub fn replace_file(
        &mut self,
        repo: &str,
        file: &str,
        content_hash: &str,
        symbols: Vec<Symbol>,
    ) -> Result<(), StoreError> {
        let now = self.clock.now_ms();
        let tx = self.conn.transaction()?;
        write_symbols(&tx, repo, file, symbols)?;
        tx.execute(
            "INSERT INTO index_files (repo_path, file_path, content_hash, indexed_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (repo_path, file_path) DO UPDATE SET \
             content_hash = excluded.content_hash, indexed_at = excluded.indexed_at",
            params![repo, file, content_hash, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records the current time as the index time of each of `files` in `repo`,
    /// for files confirmed unchanged, in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when a write fails; no time is changed then.
    pub fn restamp_files(&mut self, repo: &str, files: &[String]) -> Result<(), StoreError> {
        let now = self.clock.now_ms();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE index_files SET indexed_at = ?3 WHERE repo_path = ?1 AND file_path = ?2",
            )?;
            for file in files {
                stmt.execute(params![repo, file, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Removes every symbol and the recorded hash stored for `file` in `repo`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when a delete fails; nothing is removed then.
    pub fn clear_file(&mut self, repo: &str, file: &str) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        delete_symbols(&tx, repo, file)?;
        tx.execute(
            "DELETE FROM index_files WHERE repo_path = ?1 AND file_path = ?2",
            [repo, file],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every file indexed for `repo`, ordered by path.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the read fails.
    pub fn indexed_files(&self, repo: &str) -> Result<Vec<IndexedFileState>, StoreError> {
        let mut stmt = self.connection().prepare(
            "SELECT file_path, content_hash, indexed_at FROM index_files \
             WHERE repo_path = ?1 ORDER BY file_path",
        )?;
        let rows = stmt.query_map([repo], |row| {
            Ok(IndexedFileState {
                file_path: row.get(0)?,
                content_hash: row.get(1)?,
                indexed_at: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// File and symbol counts and the latest index time for `repo`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the read fails.
    pub fn index_status(&self, repo: &str) -> Result<IndexStatus, StoreError> {
        let conn = self.connection();
        let (files, last_indexed_at) = conn.query_row(
            "SELECT COUNT(*), MAX(indexed_at) FROM index_files WHERE repo_path = ?1",
            [repo],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
        )?;
        let symbols = conn.query_row(
            "SELECT COUNT(*) FROM symbols WHERE repo_path = ?1",
            [repo],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(IndexStatus {
            files: files.unsigned_abs(),
            symbols: symbols.unsigned_abs(),
            last_indexed_at,
        })
    }
}

fn delete_symbols(tx: &Transaction<'_>, repo: &str, file: &str) -> Result<(), StoreError> {
    tx.execute(
        "DELETE FROM symbols WHERE repo_path = ?1 AND file_path = ?2",
        [repo, file],
    )?;
    Ok(())
}

fn write_symbols(
    tx: &Transaction<'_>,
    repo: &str,
    file: &str,
    symbols: Vec<Symbol>,
) -> Result<(), StoreError> {
    delete_symbols(tx, repo, file)?;
    let mut insert = tx.prepare(
        "INSERT INTO symbols (repo_path, file_path, name, kind, line, end_line, parent) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for symbol in symbols {
        insert.execute(params![
            repo,
            file,
            symbol.name,
            symbol.kind,
            symbol.line,
            symbol.end_line,
            symbol.parent
        ])?;
    }
    Ok(())
}
