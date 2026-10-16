// SPDX-License-Identifier: AGPL-3.0-only

//! Session rows: creation, lookup, listing and model changes.

use coda_protocol::types::{BackendKind, Mode, SessionId};
use rusqlite::{OptionalExtension, Row, params};

use crate::{Store, StoreError};

/// What is needed to create a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    pub backend: BackendKind,
    pub model: Option<String>,
    pub cwd: String,
    pub mode: Mode,
}

/// A stored session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: SessionId,
    pub backend: BackendKind,
    pub model: Option<String>,
    pub cwd: String,
    pub mode: Mode,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_at: i64,
}

/// Narrows [`Store::list_sessions`]; the default matches every session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFilter {
    /// Only sessions started in this working directory.
    pub cwd: Option<String>,
}

const SESSION_COLUMNS: &str = "id, backend, model, cwd, mode, created_at";

/// Session ids are the decimal form of the row id.
pub(crate) fn session_id(row_id: i64) -> SessionId {
    SessionId::new(row_id.to_string())
}

/// The row id behind `id`, or `None` when it cannot name a session.
pub(crate) fn session_row_id(id: &SessionId) -> Option<i64> {
    id.as_str().parse().ok()
}

/// Converts a counter to SQLite's signed integer.
pub(crate) fn to_db_int(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::ValueOutOfRange { value })
}

/// Reads a non-negative SQLite integer column as a counter.
pub(crate) fn get_u64(row: &Row<'_>, idx: usize) -> Result<u64, rusqlite::Error> {
    let value: i64 = row.get(idx)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(idx, value))
}

pub(crate) fn encode_enum<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(s) => Ok(s),
        other => Ok(other.to_string()),
    }
}

pub(crate) fn decode_enum<T: serde::de::DeserializeOwned>(text: String) -> Result<T, StoreError> {
    Ok(serde_json::from_value(serde_json::Value::String(text))?)
}

fn read_session(row: &Row<'_>) -> Result<Result<SessionRecord, StoreError>, rusqlite::Error> {
    let id: i64 = row.get(0)?;
    let backend: String = row.get(1)?;
    let mode: String = row.get(4)?;
    let model = row.get(2)?;
    let cwd = row.get(3)?;
    let created_at = row.get(5)?;
    Ok(decode_enum(backend).and_then(|backend| {
        Ok(SessionRecord {
            id: session_id(id),
            backend,
            model,
            cwd,
            mode: decode_enum(mode)?,
            created_at,
        })
    }))
}

impl Store {
    /// Creates a session and returns its id.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the insert fails.
    pub fn create_session(&mut self, new: &NewSession) -> Result<SessionId, StoreError> {
        self.conn.execute(
            "INSERT INTO sessions (created_at, backend, model, cwd, mode) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                self.clock.now_ms(),
                encode_enum(&new.backend)?,
                new.model,
                new.cwd,
                encode_enum(&new.mode)?,
            ],
        )?;
        Ok(session_id(self.conn.last_insert_rowid()))
    }

    /// Loads one session.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] when no such session exists.
    pub fn load_session(&self, id: &SessionId) -> Result<SessionRecord, StoreError> {
        let not_found = || StoreError::SessionNotFound {
            session: id.clone(),
        };
        let row_id = session_row_id(id).ok_or_else(not_found)?;
        self.conn
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
                [row_id],
                read_session,
            )
            .optional()?
            .ok_or_else(not_found)?
    }

    /// Lists sessions matching `filter`, oldest first. No match is an empty list.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the query fails.
    pub fn list_sessions(&self, filter: &SessionFilter) -> Result<Vec<SessionRecord>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions \
             WHERE (?1 IS NULL OR cwd = ?1) ORDER BY id"
        ))?;
        let rows = stmt.query_map([&filter.cwd], read_session)?;
        rows.map(|row| row?).collect()
    }

    /// Changes the model a session uses.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] when no such session exists.
    pub fn set_session_model(&mut self, id: &SessionId, model: &str) -> Result<(), StoreError> {
        let changed = match session_row_id(id) {
            Some(row_id) => self.conn.execute(
                "UPDATE sessions SET model = ?1 WHERE id = ?2",
                params![model, row_id],
            )?,
            None => 0,
        };
        if changed == 0 {
            return Err(StoreError::SessionNotFound {
                session: id.clone(),
            });
        }
        Ok(())
    }
}
