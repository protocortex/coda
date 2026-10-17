// SPDX-License-Identifier: AGPL-3.0-only

//! Read-only views the protocol layer needs from storage and indexing.
//!
//! The traits are synchronous so this crate stays free of an async runtime.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use crate::types::OriginalId;

/// Why a read failed.
#[derive(Debug, Error)]
pub enum ReaderError {
    /// No stored item has the requested identifier.
    #[error("original `{0}` was not found")]
    NotFound(String),
    /// The item existed but was dropped to keep the session within its storage limit.
    #[error("original `{0}` was evicted to keep the session within its storage limit")]
    Evicted(String),
    /// The backing store cannot answer right now.
    #[error("index is unavailable: {0}")]
    Unavailable(String),
}

/// A named code symbol and where it is defined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolLocation {
    /// Symbol name.
    pub name: String,
    /// Symbol kind, such as `fn` or `struct`.
    pub kind: String,
    /// File path relative to the workspace root.
    pub path: String,
    /// One-based line of the definition.
    pub line: u32,
}

/// One match from a text search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// File path relative to the workspace root.
    pub path: String,
    /// One-based line of the match.
    pub line: u32,
    /// The matching line, trimmed.
    pub snippet: String,
}

/// Fetches full tool outputs that were shortened before reaching the model.
pub trait OriginalsReader {
    /// Returns the untruncated output stored under `id`.
    ///
    /// # Errors
    /// [`ReaderError::NotFound`] when nothing is stored under `id` and
    /// [`ReaderError::Evicted`] when it was dropped to make room.
    fn fetch_original(&self, id: &OriginalId) -> Result<String, ReaderError>;
}

/// Answers symbol, outline and search queries over the workspace index.
pub trait IndexReader {
    /// Finds symbols whose name matches `query`.
    ///
    /// # Errors
    /// [`ReaderError::Unavailable`] when the index cannot answer.
    fn symbols(&self, query: &str) -> Result<Vec<SymbolLocation>, ReaderError>;

    /// Lists the symbols defined in the file at `path`, in source order.
    ///
    /// # Errors
    /// [`ReaderError::Unavailable`] when the index cannot answer.
    fn outline(&self, path: &str) -> Result<Vec<SymbolLocation>, ReaderError>;

    /// Searches indexed text for `query`, returning at most `limit` hits.
    ///
    /// # Errors
    /// [`ReaderError::Unavailable`] when the index cannot answer.
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, ReaderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;

    impl OriginalsReader for Fake {
        fn fetch_original(&self, id: &OriginalId) -> Result<String, ReaderError> {
            if id.as_str() == "known" {
                Ok("full output".to_owned())
            } else {
                Err(ReaderError::NotFound(id.to_string()))
            }
        }
    }

    impl IndexReader for Fake {
        fn symbols(&self, query: &str) -> Result<Vec<SymbolLocation>, ReaderError> {
            Ok(vec![SymbolLocation {
                name: query.to_owned(),
                kind: "fn".to_owned(),
                path: "src/lib.rs".to_owned(),
                line: 3,
            }])
        }

        fn outline(&self, _path: &str) -> Result<Vec<SymbolLocation>, ReaderError> {
            Ok(Vec::new())
        }

        fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchHit>, ReaderError> {
            Err(ReaderError::Unavailable("index is building".to_owned()))
        }
    }

    #[test]
    fn originals_reader_is_object_safe_and_returns_known_output() {
        // Arrange
        let reader: Box<dyn OriginalsReader> = Box::new(Fake);

        // Act
        let found = reader.fetch_original(&OriginalId::new("known"));

        // Assert
        assert_eq!(found.unwrap(), "full output");
    }

    #[test]
    fn originals_reader_reports_not_found_with_the_id() {
        // Arrange
        let reader = Fake;

        // Act
        let err = reader.fetch_original(&OriginalId::new("gone")).unwrap_err();

        // Assert
        assert_eq!(err.to_string(), "original `gone` was not found");
    }

    #[test]
    fn index_reader_is_object_safe_and_reports_unavailable() {
        // Arrange
        let reader: Box<dyn IndexReader> = Box::new(Fake);

        // Act
        let symbols = reader.symbols("main").unwrap();
        let err = reader.search("main", 5).unwrap_err();

        // Assert
        assert_eq!(symbols[0].name, "main");
        assert_eq!(err.to_string(), "index is unavailable: index is building");
    }
}
