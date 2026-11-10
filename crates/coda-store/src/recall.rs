// SPDX-License-Identifier: Apache-2.0

//! Full-text search over the messages and tool summaries of earlier sessions.
//!
//! The index copies only text the store already holds, and only while recall
//! is switched on. It lives and dies with the rows it was made from: a deleted
//! session, a deleted original or an off switch removes it.

use coda_protocol::readers::{ReaderError, RecallHit, RecallReader};
use coda_protocol::types::{BackendKind, SessionId, TurnId};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::sessions::{decode_enum, get_u64, session_id, session_row_id};
use crate::{Store, StoreError};

/// Most hits one search returns, whatever limit the caller asks for.
pub const MAX_RECALL_RESULTS: usize = 20;
/// Most bytes one snippet holds.
pub const MAX_RECALL_SNIPPET_BYTES: usize = 400;

const USER_MESSAGE_KIND: &str = "user_message";
const ASSISTANT_MESSAGE_KIND: &str = "assistant_message";
const TOOL_FINISHED_KIND: &str = "tool_finished";
/// Bytes of one message that are indexed; the rest is not searchable.
const MAX_INDEXED_MESSAGE_BYTES: usize = 16 * 1024;
/// Bytes of a tool result that make its summary.
const MAX_INDEXED_SUMMARY_BYTES: i64 = 2 * 1024;
/// Events indexed per transaction when recall is switched on over a stored history.
const BACKFILL_PAGE: i64 = 500;
/// Words of context around a match, at most; FTS5 allows 64.
const SNIPPET_WORDS: u32 = 32;
const SNIPPET_OPEN: &str = "[";
const SNIPPET_CLOSE: &str = "]";
const SNIPPET_GAP: &str = "...";

fn unavailable(error: impl std::fmt::Display) -> ReaderError {
    ReaderError::Unavailable(error.to_string())
}

/// Quotes each word of user text as an FTS5 string, so query syntax is never
/// interpreted. A text matches on any of its words and the best match comes
/// first; the last word matches as a prefix. Text with no word gives `None`.
fn fts_words(query: &str) -> Option<String> {
    let mut words: Vec<String> = query
        .split_whitespace()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect();
    words.last_mut()?.push('*');
    Some(words.join(" OR "))
}

/// The longest prefix of `text` within `max` bytes that ends on a character.
fn cut(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// What an event adds to the index: its text, and the original the text was
/// made from when it was.
struct Entry {
    text: String,
    original: Option<i64>,
}

fn entry_of(conn: &Connection, session_row: i64, kind: &str, body: &str) -> Option<Entry> {
    let value: Value = serde_json::from_str(body).ok()?;
    match kind {
        USER_MESSAGE_KIND | ASSISTANT_MESSAGE_KIND => {
            let text = value.get("text")?.as_str()?.trim();
            (!text.is_empty()).then(|| Entry {
                text: cut(text, MAX_INDEXED_MESSAGE_BYTES).to_owned(),
                original: None,
            })
        }
        TOOL_FINISHED_KIND => {
            let tool = value.get("tool")?.as_str()?;
            let original: i64 = value.get("original")?.as_str()?.parse().ok()?;
            let bytes: Vec<u8> = conn
                .query_row(
                    "SELECT substr(content, 1, ?3) FROM originals WHERE id = ?1 AND session_id = ?2",
                    params![original, session_row, MAX_INDEXED_SUMMARY_BYTES],
                    |row| row.get(0),
                )
                .optional()
                .ok()??;
            let result = String::from_utf8_lossy(&bytes);
            let result = result.trim_end_matches('\u{FFFD}').trim();
            (!result.is_empty()).then(|| Entry {
                text: format!("{tool}: {result}"),
                original: Some(original),
            })
        }
        _ => None,
    }
}

/// An appended event, as the index sees it.
pub(crate) struct Indexed<'a> {
    pub session_row: i64,
    pub turn_row: i64,
    pub event_row: i64,
    pub kind: &'a str,
    pub body: &'a str,
    pub at: i64,
}

/// Copies an event into the index when it holds text worth searching.
pub(crate) fn index_event(tx: &Transaction<'_>, event: &Indexed<'_>) -> Result<(), StoreError> {
    let Some(entry) = entry_of(tx, event.session_row, event.kind, event.body) else {
        return Ok(());
    };
    tx.execute(
        "INSERT OR IGNORE INTO recall_entries \
         (session_id, turn_id, event_id, original_id, kind, body, at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            event.session_row,
            event.turn_row,
            event.event_row,
            entry.original,
            event.kind,
            entry.text,
            event.at
        ],
    )?;
    Ok(())
}

/// Removes the summary made from the original `original_row`.
pub(crate) fn forget_original(conn: &Connection, original_row: i64) -> Result<(), StoreError> {
    conn.execute(
        "DELETE FROM recall_entries WHERE original_id = ?1",
        [original_row],
    )?;
    Ok(())
}

struct StoredEvent {
    event_row: i64,
    session_row: i64,
    turn_row: i64,
    kind: String,
    body: String,
    at: i64,
}

/// The next page of events after `after` that are not indexed yet.
fn pending_events(tx: &Transaction<'_>, after: i64) -> Result<Vec<StoredEvent>, StoreError> {
    let mut stmt = tx.prepare(
        "SELECT e.id, t.session_id, e.turn_id, e.kind, e.body, e.at FROM events e \
         JOIN turns t ON t.id = e.turn_id \
         WHERE e.id > ?4 AND e.kind IN (?1, ?2, ?3) \
           AND NOT EXISTS (SELECT 1 FROM recall_entries r WHERE r.event_id = e.id) \
         ORDER BY e.id LIMIT ?5",
    )?;
    let rows = stmt.query_map(
        params![
            USER_MESSAGE_KIND,
            ASSISTANT_MESSAGE_KIND,
            TOOL_FINISHED_KIND,
            after,
            BACKFILL_PAGE
        ],
        |row| {
            Ok(StoredEvent {
                event_row: row.get(0)?,
                session_row: row.get(1)?,
                turn_row: row.get(2)?,
                kind: row.get(3)?,
                body: row.get(4)?,
                at: row.get(5)?,
            })
        },
    )?;
    Ok(rows.collect::<Result<_, _>>()?)
}

impl Store {
    /// Switches the recall index on or off for this handle.
    ///
    /// On, every message and tool summary appended from now on is indexed, and
    /// the ones stored earlier that are not yet indexed are indexed now. Off,
    /// nothing is indexed and everything indexed so far is removed. Calling it
    /// again with the same value changes nothing more.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the index cannot be written.
    pub fn configure_recall(&mut self, enabled: bool) -> Result<(), StoreError> {
        self.recall = enabled;
        if !enabled {
            self.conn.execute("DELETE FROM recall_entries", [])?;
            return Ok(());
        }
        let mut after = 0_i64;
        loop {
            let tx = self.conn.transaction()?;
            let pending = pending_events(&tx, after)?;
            let Some(last) = pending.last() else {
                return Ok(());
            };
            after = last.event_row;
            for event in &pending {
                index_event(
                    &tx,
                    &Indexed {
                        session_row: event.session_row,
                        turn_row: event.turn_row,
                        event_row: event.event_row,
                        kind: &event.kind,
                        body: &event.body,
                        at: event.at,
                    },
                )?;
            }
            tx.commit()?;
        }
    }

    /// A search over the earlier sessions of the repository `asker` belongs to.
    /// Sessions of a delegated backend are searched only when that repository
    /// is in `delegated_repos`.
    #[must_use]
    pub fn recall_reader(&self, asker: &SessionId, delegated_repos: &[String]) -> RecallIndex<'_> {
        RecallIndex {
            store: self,
            asker: asker.clone(),
            delegated_repos: delegated_repos.to_vec(),
        }
    }
}

/// The recall index as a read-only search, bound to one session's repository.
pub struct RecallIndex<'a> {
    store: &'a Store,
    asker: SessionId,
    delegated_repos: Vec<String>,
}

impl RecallIndex<'_> {
    fn repo_of_asker(&self) -> Result<Option<String>, StoreError> {
        let Some(row) = session_row_id(&self.asker) else {
            return Ok(None);
        };
        Ok(self
            .store
            .connection()
            .query_row(
                "SELECT COALESCE(worktree_repo, cwd) FROM sessions WHERE id = ?1",
                [row],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn find(&self, query: &str, limit: usize) -> Result<Vec<RecallHit>, StoreError> {
        let Some(words) = fts_words(query) else {
            return Ok(Vec::new());
        };
        let Some(repo) = self.repo_of_asker()? else {
            return Ok(Vec::new());
        };
        let delegated_allowed = self
            .delegated_repos
            .iter()
            .any(|listed| listed.trim_end_matches('/') == repo.trim_end_matches('/'));
        let limit = limit.min(MAX_RECALL_RESULTS);
        let mut stmt = self.store.connection().prepare(&format!(
            "SELECT s.id, r.turn_id, e.seq, r.kind, \
                    snippet(recall_fts, 0, '{SNIPPET_OPEN}', '{SNIPPET_CLOSE}', '{SNIPPET_GAP}', {SNIPPET_WORDS}), \
                    r.at, s.backend \
             FROM recall_fts \
             JOIN recall_entries r ON r.id = recall_fts.rowid \
             JOIN events e ON e.id = r.event_id \
             JOIN turns t ON t.id = r.turn_id \
             JOIN sessions s ON s.id = r.session_id \
             WHERE recall_fts MATCH ?1 \
               AND COALESCE(s.worktree_repo, s.cwd) = ?2 \
               AND t.rolled_back_at IS NULL \
             ORDER BY rank, r.id DESC"
        ))?;
        let mut rows = stmt.query(params![words, repo])?;
        let mut hits = Vec::new();
        while hits.len() < limit {
            let Some(row) = rows.next()? else {
                break;
            };
            let backend: BackendKind = decode_enum(row.get(6)?)?;
            if !backend.is_model_backend() && !delegated_allowed {
                continue;
            }
            let snippet: String = row.get(4)?;
            hits.push(RecallHit {
                session: session_id(row.get(0)?),
                turn: TurnId::new(row.get::<_, i64>(1)?.to_string()),
                seq: get_u64(row, 2)?,
                kind: row.get(3)?,
                snippet: cut(&snippet, MAX_RECALL_SNIPPET_BYTES).to_owned(),
                at: row.get(5)?,
            });
        }
        Ok(hits)
    }
}

impl RecallReader for RecallIndex<'_> {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<RecallHit>, ReaderError> {
        self.find(query, limit).map_err(unavailable)
    }
}
