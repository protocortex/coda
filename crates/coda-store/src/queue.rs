// SPDX-License-Identifier: Apache-2.0

//! Follow-up messages that wait for a session's running turn to end.

use coda_protocol::types::{SessionId, TurnId};
use rusqlite::{OptionalExtension, params};

use crate::sessions::{session_row_id, to_db_int};
use crate::{Store, StoreError};

/// Most messages one session may have waiting.
pub const MAX_QUEUED_MESSAGES: usize = 32;

/// Most bytes of text one queued message may hold, so a listing of the queue
/// always fits in one protocol frame.
pub const MAX_QUEUED_TEXT_BYTES: usize = 64 * 1024;

/// A message to add to the queue, with the limits its turn should run under.
#[derive(Debug, Clone, Copy)]
pub struct NewQueued<'a> {
    pub text: &'a str,
    pub max_turns: Option<u32>,
    pub token_cap: Option<u64>,
}

/// A message waiting for its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedMessage {
    /// Identifies the message within the store; grows with each message.
    pub id: i64,
    pub text: String,
    /// Most agent turns the message's turn may take, as the sender asked.
    pub max_turns: Option<u32>,
    /// Most tokens the message's turn may spend, as the sender asked.
    pub token_cap: Option<u64>,
    /// Time it was queued, in milliseconds since the Unix epoch.
    pub at: i64,
}

impl Store {
    /// Adds `text` behind the messages already waiting in `session`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`], [`StoreError::QueueFull`]
    /// when the session already has [`MAX_QUEUED_MESSAGES`] waiting, and
    /// [`StoreError::QueuedTextTooLarge`] for text past [`MAX_QUEUED_TEXT_BYTES`].
    pub fn enqueue_message(
        &mut self,
        session: &SessionId,
        message: NewQueued<'_>,
    ) -> Result<QueuedMessage, StoreError> {
        if message.text.len() > MAX_QUEUED_TEXT_BYTES {
            return Err(StoreError::QueuedTextTooLarge {
                limit: MAX_QUEUED_TEXT_BYTES,
            });
        }
        let row = self.existing_session_row(session)?;
        let at = self.clock.now_ms();
        let tx = self.conn.transaction()?;
        let waiting: i64 = tx.query_row(
            "SELECT COUNT(*) FROM queued_messages WHERE session_id = ?1",
            [row],
            |r| r.get(0),
        )?;
        if usize::try_from(waiting).is_ok_and(|n| n >= MAX_QUEUED_MESSAGES) {
            return Err(StoreError::QueueFull {
                session: session.clone(),
                limit: MAX_QUEUED_MESSAGES,
            });
        }
        tx.execute(
            "INSERT INTO queued_messages (session_id, text, max_turns, token_cap, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row,
                message.text,
                message.max_turns,
                message.token_cap.map(to_db_int).transpose()?,
                at
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(QueuedMessage {
            id,
            text: message.text.to_owned(),
            max_turns: message.max_turns,
            token_cap: message.token_cap,
            at,
        })
    }

    /// The messages waiting in `session`, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the query fails.
    pub fn queued_messages(&self, session: &SessionId) -> Result<Vec<QueuedMessage>, StoreError> {
        let Some(row) = session_row_id(session) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT id, text, max_turns, token_cap, created_at FROM queued_messages \
             WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([row], |r| {
            Ok(QueuedMessage {
                id: r.get(0)?,
                text: r.get(1)?,
                max_turns: r.get(2)?,
                token_cap: r
                    .get::<_, Option<i64>>(3)?
                    .and_then(|v| u64::try_from(v).ok()),
                at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// How many turns of `session` began with a user message of exactly `text`
    /// within the last `window_ms` milliseconds.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the query fails.
    pub fn recent_user_messages(
        &self,
        session: &SessionId,
        text: &str,
        window_ms: i64,
    ) -> Result<u32, StoreError> {
        let Some(row) = session_row_id(session) else {
            return Ok(0);
        };
        let since = self.clock.now_ms().saturating_sub(window_ms);
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM events e JOIN turns t ON t.id = e.turn_id \
             WHERE t.session_id = ?1 AND e.kind = 'user_message' \
             AND json_extract(e.body, '$.text') = ?2 AND e.at >= ?3",
            params![row, text, since],
            |r| r.get::<_, u32>(0),
        )?;
        Ok(count)
    }

    /// The sessions that hold a waiting message of exactly `text`, oldest session first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the query fails.
    pub fn sessions_waiting_with(&self, text: &str) -> Result<Vec<SessionId>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT session_id FROM queued_messages WHERE text = ?1 ORDER BY session_id",
        )?;
        let rows = stmt.query_map([text], |r| r.get::<_, i64>(0))?;
        let ids = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(ids
            .into_iter()
            .map(|id| SessionId::new(id.to_string()))
            .collect())
    }

    /// Removes one waiting message. Returns whether it was there, so a second
    /// removal, or one for another session's message, is not an error.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the write fails.
    pub fn remove_queued_message(
        &mut self,
        session: &SessionId,
        id: i64,
    ) -> Result<bool, StoreError> {
        let Some(row) = session_row_id(session) else {
            return Ok(false);
        };
        let removed = self.conn.execute(
            "DELETE FROM queued_messages WHERE id = ?1 AND session_id = ?2",
            params![id, row],
        )?;
        Ok(removed > 0)
    }

    /// The newest turn of `session`, or `None` when it never had one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the query fails.
    pub fn latest_turn(&self, session: &SessionId) -> Result<Option<TurnId>, StoreError> {
        let Some(row) = session_row_id(session) else {
            return Ok(None);
        };
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(id) FROM turns WHERE session_id = ?1",
                [row],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(id.map(|id| TurnId::new(id.to_string())))
    }
}
