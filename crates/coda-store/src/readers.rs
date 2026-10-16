// SPDX-License-Identifier: AGPL-3.0-only

//! The store as the protocol layer's read-only view of originals and the index.

use coda_protocol::readers::{
    IndexReader, OriginalsReader, ReaderError, SearchHit, SymbolLocation,
};
use coda_protocol::types::{OriginalId, SessionId};
use rusqlite::{Row, params};

use crate::{Store, StoreError};

/// Most rows a symbol-name lookup returns.
const MAX_SYMBOL_RESULTS: usize = 100;

fn unavailable(error: impl std::fmt::Display) -> ReaderError {
    ReaderError::Unavailable(error.to_string())
}

fn limit_to_sql(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

/// Quotes user text as one FTS5 phrase so query syntax is never interpreted;
/// the last term matches as a prefix.
fn fts_phrase(query: &str) -> String {
    format!("\"{}\"*", query.replace('"', "\"\""))
}

fn symbol_location(row: &Row<'_>) -> rusqlite::Result<SymbolLocation> {
    Ok(SymbolLocation {
        name: row.get(0)?,
        kind: row.get(1)?,
        path: row.get(2)?,
        line: row.get(3)?,
    })
}

/// Reads originals of one session; ids from any other session are not found.
pub struct SessionOriginals<'a> {
    store: &'a Store,
    session: SessionId,
}

/// Reads the symbol index of one repo; symbols of other repos are never returned.
pub struct RepoIndex<'a> {
    store: &'a Store,
    repo_path: String,
}

impl Store {
    /// A reader bound to `session` for the protocol layer.
    #[must_use]
    pub fn originals_reader(&self, session: &SessionId) -> SessionOriginals<'_> {
        SessionOriginals {
            store: self,
            session: session.clone(),
        }
    }

    /// A reader bound to `repo_path` for the protocol layer.
    #[must_use]
    pub fn index_reader(&self, repo_path: &str) -> RepoIndex<'_> {
        RepoIndex {
            store: self,
            repo_path: repo_path.to_owned(),
        }
    }
}

impl RepoIndex<'_> {
    fn match_symbols(&self, query: &str, limit: usize) -> Result<Vec<SymbolLocation>, StoreError> {
        let mut stmt = self.store.connection().prepare(
            "SELECT s.name, s.kind, s.file_path, s.line FROM symbols_fts \
             JOIN symbols s ON s.id = symbols_fts.rowid \
             WHERE symbols_fts MATCH ?1 AND s.repo_path = ?2 \
             ORDER BY rank, s.id LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![fts_phrase(query), self.repo_path, limit_to_sql(limit)],
            symbol_location,
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn file_outline(&self, path: &str) -> Result<Vec<SymbolLocation>, StoreError> {
        let mut stmt = self.store.connection().prepare(
            "SELECT name, kind, file_path, line FROM symbols \
             WHERE repo_path = ?1 AND file_path = ?2 ORDER BY line, id",
        )?;
        let rows = stmt.query_map([self.repo_path.as_str(), path], symbol_location)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

impl OriginalsReader for SessionOriginals<'_> {
    fn fetch_original(&self, id: &OriginalId) -> Result<String, ReaderError> {
        self.store
            .load_original(&self.session, id)
            .map_err(unavailable)?
            .ok_or_else(|| ReaderError::NotFound(id.to_string()))
    }
}

impl IndexReader for RepoIndex<'_> {
    fn symbols(&self, query: &str) -> Result<Vec<SymbolLocation>, ReaderError> {
        self.match_symbols(query, MAX_SYMBOL_RESULTS)
            .map_err(unavailable)
    }

    fn outline(&self, path: &str) -> Result<Vec<SymbolLocation>, ReaderError> {
        self.file_outline(path).map_err(unavailable)
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, ReaderError> {
        let hits = self.match_symbols(query, limit).map_err(unavailable)?;
        Ok(hits
            .into_iter()
            .map(|s| SearchHit {
                path: s.path,
                line: s.line,
                snippet: s.name,
            })
            .collect())
    }
}
