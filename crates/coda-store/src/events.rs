// SPDX-License-Identifier: AGPL-3.0-only

//! Turns and the per-session event log.
//!
//! Each session has one writer at a time. A [`Store`] handle takes a lease on
//! a session the first time an append passes its session and turn checks, and
//! a second handle appending to that session gets [`StoreError::WriterBusy`]
//! instead of waiting. An append that fails those checks takes no lease. The lease lives
//! in a process-wide registry keyed by database file and session, and is
//! released when the owning handle drops. It guards handles within one
//! process, which is how the daemon uses the store.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use coda_protocol::types::{SessionId, TurnId};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, ffi, params};

use crate::sessions::{get_u64, session_row_id, to_db_int};
use crate::{Store, StoreError};

type LeaseKey = (PathBuf, i64);

static WRITER_LEASES: Mutex<Option<HashMap<LeaseKey, u64>>> = Mutex::new(None);

fn leases() -> MutexGuard<'static, Option<HashMap<LeaseKey, u64>>> {
    // The map holds plain data, so a panic elsewhere cannot leave it invalid.
    WRITER_LEASES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An event to append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRecord {
    pub kind: String,
    pub body: String,
}

/// An event as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    /// Position in the session, starting at 1.
    pub seq: u64,
    pub turn: TurnId,
    pub kind: String,
    pub body: String,
    /// Append time in milliseconds since the Unix epoch.
    pub at: i64,
}

fn turn_id(row_id: i64) -> TurnId {
    TurnId::new(row_id.to_string())
}

impl Store {
    /// Starts a new turn in `session`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] when the session does not exist.
    pub fn begin_turn(&mut self, session: &SessionId) -> Result<TurnId, StoreError> {
        let session_row = self.existing_session_row(session)?;
        self.conn.execute(
            "INSERT INTO turns (session_id, started_at) VALUES (?1, ?2)",
            params![session_row, self.clock.now_ms()],
        )?;
        Ok(turn_id(self.conn.last_insert_rowid()))
    }

    /// Appends an event to `session` under the next sequence number, which is
    /// returned. Sequence numbers run 1, 2, 3 across all turns of the session.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`], [`StoreError::TurnNotFound`]
    /// when the turn is not part of the session, or [`StoreError::WriterBusy`]
    /// when another handle writes to the session.
    pub fn append_event(
        &mut self,
        session: &SessionId,
        turn: &TurnId,
        record: &EventRecord,
    ) -> Result<u64, StoreError> {
        self.append(session, turn, None, record)
    }

    /// Appends an event under an explicit sequence number, for replaying a
    /// log. Appending a number the session already has changes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ValueOutOfRange`] when `seq` is 0, since numbering
    /// starts at 1, [`StoreError::DuplicateAppend`] when `seq` is taken, plus the
    /// errors of [`Store::append_event`].
    pub fn append_event_at(
        &mut self,
        session: &SessionId,
        turn: &TurnId,
        seq: u64,
        record: &EventRecord,
    ) -> Result<u64, StoreError> {
        self.append(session, turn, Some(seq), record)
    }

    /// Loads up to `limit` events of `session` with a sequence number above
    /// `after_seq`, in sequence order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] when the session does not exist.
    pub fn load_events(
        &self,
        session: &SessionId,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let session_row = self.existing_session_row(session)?;
        let mut stmt = self.conn.prepare(
            "SELECT e.seq, e.turn_id, e.kind, e.body, e.at FROM events e \
             JOIN turns t ON t.id = e.turn_id \
             WHERE t.session_id = ?1 AND e.seq > ?2 ORDER BY e.seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![session_row, to_db_int(after_seq)?, limit], |row| {
            Ok(StoredEvent {
                seq: get_u64(row, 0)?,
                turn: turn_id(row.get(1)?),
                kind: row.get(2)?,
                body: row.get(3)?,
                at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub(crate) fn existing_session_row(&self, session: &SessionId) -> Result<i64, StoreError> {
        let not_found = || StoreError::SessionNotFound {
            session: session.clone(),
        };
        let row_id = session_row_id(session).ok_or_else(not_found)?;
        self.conn
            .query_row("SELECT 1 FROM sessions WHERE id = ?1", [row_id], |_| Ok(()))
            .optional()?
            .map(|()| row_id)
            .ok_or_else(not_found)
    }

    fn append(
        &mut self,
        session: &SessionId,
        turn: &TurnId,
        explicit_seq: Option<u64>,
        record: &EventRecord,
    ) -> Result<u64, StoreError> {
        if explicit_seq == Some(0) {
            return Err(StoreError::ValueOutOfRange { value: 0 });
        }
        let session_row = self.existing_session_row(session)?;
        let at = self.clock.now_ms();
        // IMMEDIATE takes the write lock up front, so the sequence number
        // read below cannot be taken by another connection before the insert.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let turn_row = owned_turn_row(&tx, session, session_row, turn)?;
        claim_writer_lease(&self.db_path, self.id, session, session_row)?;
        let seq = match explicit_seq {
            Some(seq) => seq,
            None => next_seq(&tx, session_row)?,
        };
        insert_event(&tx, session, turn_row, seq, record, at)?;
        tx.commit()?;
        Ok(seq)
    }
}

fn claim_writer_lease(
    db_path: &Path,
    store_id: u64,
    session: &SessionId,
    session_row: i64,
) -> Result<(), StoreError> {
    let mut guard = leases();
    let owner = *guard
        .get_or_insert_default()
        .entry((db_path.to_path_buf(), session_row))
        .or_insert(store_id);
    if owner == store_id {
        Ok(())
    } else {
        Err(StoreError::WriterBusy {
            session: session.clone(),
        })
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if let Some(map) = leases().as_mut() {
            map.retain(|_, owner| *owner != self.id);
        }
    }
}

/// The row id of `turn`, checking that it belongs to `session`.
pub(crate) fn owned_turn_row(
    conn: &Connection,
    session: &SessionId,
    session_row: i64,
    turn: &TurnId,
) -> Result<i64, StoreError> {
    let not_found = || StoreError::TurnNotFound {
        session: session.clone(),
        turn: turn.clone(),
    };
    let turn_row = turn.as_str().parse::<i64>().map_err(|_| not_found())?;
    conn.query_row(
        "SELECT 1 FROM turns WHERE id = ?1 AND session_id = ?2",
        [turn_row, session_row],
        |_| Ok(()),
    )
    .optional()?
    .map(|()| turn_row)
    .ok_or_else(not_found)
}

fn next_seq(tx: &Transaction<'_>, session_row: i64) -> Result<u64, StoreError> {
    let last = tx.query_row(
        "SELECT COALESCE(MAX(e.seq), 0) FROM events e \
         JOIN turns t ON t.id = e.turn_id WHERE t.session_id = ?1",
        [session_row],
        |row| get_u64(row, 0),
    )?;
    Ok(last + 1)
}

fn insert_event(
    tx: &Transaction<'_>,
    session: &SessionId,
    turn_row: i64,
    seq: u64,
    record: &EventRecord,
    at: i64,
) -> Result<(), StoreError> {
    let taken: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM events e JOIN turns t ON t.id = e.turn_id \
         WHERE t.session_id = (SELECT session_id FROM turns WHERE id = ?1) AND e.seq = ?2)",
        params![turn_row, to_db_int(seq)?],
        |row| row.get(0),
    )?;
    let duplicate = || StoreError::DuplicateAppend {
        session: session.clone(),
        seq,
    };
    if taken {
        return Err(duplicate());
    }
    tx.execute(
        "INSERT INTO events (turn_id, seq, kind, body, at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![turn_row, to_db_int(seq)?, record.kind, record.body, at],
    )
    .map_err(|e| match e.sqlite_error() {
        Some(failure) if failure.extended_code == ffi::SQLITE_CONSTRAINT_UNIQUE => duplicate(),
        _ => e.into(),
    })?;
    Ok(())
}
