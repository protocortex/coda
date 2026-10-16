// SPDX-License-Identifier: AGPL-3.0-only

//! The content-free event log: ids, an allowlisted kind and timestamps only.

use rusqlite::params;

use crate::sessions::to_db_int;
use crate::{Store, StoreError};

/// Largest id the log accepts for a session, turn or producer.
pub const MAX_LOG_ID: u64 = u32::MAX as u64;

/// What happened. Mirrors the `CHECK` list on `event_log.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LogEventKind {
    SessionStarted,
    SessionEnded,
    TurnStarted,
    TurnEnded,
    ToolCalled,
    PermissionRequested,
    PermissionDecided,
    IndexUpdated,
    Error,
}

impl LogEventKind {
    fn as_sql(self) -> &'static str {
        match self {
            Self::SessionStarted => "session_started",
            Self::SessionEnded => "session_ended",
            Self::TurnStarted => "turn_started",
            Self::TurnEnded => "turn_ended",
            Self::ToolCalled => "tool_called",
            Self::PermissionRequested => "permission_requested",
            Self::PermissionDecided => "permission_decided",
            Self::IndexUpdated => "index_updated",
            Self::Error => "error",
        }
    }
}

/// How widely an event applies. Mirrors the `CHECK` list on `event_log.scope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LogScope {
    Session,
    Repo,
    Global,
}

impl LogScope {
    fn as_sql(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Repo => "repo",
            Self::Global => "global",
        }
    }
}

/// One content-free log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEvent {
    pub kind: LogEventKind,
    pub session: Option<u64>,
    pub turn: Option<u64>,
    pub scope: Option<LogScope>,
    pub producer_id: Option<u64>,
}

fn checked_id(field: &'static str, value: Option<u64>) -> Result<Option<i64>, StoreError> {
    match value {
        Some(value) if value > MAX_LOG_ID => Err(StoreError::LogValueTooLarge {
            field,
            value,
            max: MAX_LOG_ID,
        }),
        Some(value) => to_db_int(value).map(Some),
        None => Ok(None),
    }
}

impl Store {
    /// Appends an entry to the event log, stamped with the store clock.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::LogValueTooLarge`] when an id exceeds
    /// [`MAX_LOG_ID`], and [`StoreError::Sqlite`] when the insert fails.
    pub fn record_log_event(&mut self, event: &LogEvent) -> Result<(), StoreError> {
        let session = checked_id("session", event.session)?;
        let turn = checked_id("turn", event.turn)?;
        let producer = checked_id("producer_id", event.producer_id)?;
        self.connection().execute(
            "INSERT INTO event_log (kind, at, session_id, turn_id, scope, producer_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.kind.as_sql(),
                self.clock.now_ms(),
                session,
                turn,
                event.scope.map(LogScope::as_sql),
                producer
            ],
        )?;
        Ok(())
    }
}
