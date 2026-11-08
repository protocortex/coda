// SPDX-License-Identifier: Apache-2.0

//! Scheduled prompts: the rows, the claim of a due run and the recovery after a crash.
//!
//! A run is claimed in one transaction that sets the row `running` and moves its next
//! run forward, so two ticks never claim the same run and a crash leaves a row that
//! [`Store::recover_schedules`] finds.

use coda_protocol::methods::{
    MAX_SCHEDULES, MIN_SCHEDULE_INTERVAL_SECS, ScheduleInfo, ScheduleSpec, ScheduleState,
    ScheduleTarget,
};
use rusqlite::{Row, TransactionBehavior, params};

use crate::schedule_time::{first_run, next_after, weekday_mask, weekdays_of};
use crate::sessions::{decode_enum, encode_enum, get_u64, session_id, session_row_id};
use crate::{Store, StoreError};

/// Longest prompt a schedule keeps.
pub const MAX_SCHEDULE_TEXT_BYTES: usize = 16 * 1024;
/// Longest gap between two runs of an interval schedule, one year.
pub const MAX_SCHEDULE_INTERVAL_SECS: u64 = 365 * 24 * 60 * 60;
const MINUTES_PER_DAY: u32 = 24 * 60;
/// Most schedules `list_schedules` returns: the live ones first, then the newest done ones.
pub const MAX_LISTED_SCHEDULES: usize = 200;
/// Longest error kept on a schedule row, in characters.
pub const MAX_SCHEDULE_ERROR_CHARS: usize = 300;

const COLUMNS: &str = "id, kind, at, every_secs, minute_of_day, weekdays, target_session, \
                       backend, model, cwd, mode, text, state, next_run_at, last_run_at, runs, \
                       created_at, last_error, last_error_at";

fn refused(reason: impl Into<String>) -> StoreError {
    StoreError::ScheduleRefused {
        reason: reason.into(),
    }
}

fn check(spec: &ScheduleSpec, target: &ScheduleTarget, text: &str) -> Result<(), StoreError> {
    if text.trim().is_empty() {
        return Err(refused("the prompt is empty"));
    }
    if text.len() > MAX_SCHEDULE_TEXT_BYTES {
        return Err(refused(format!(
            "the prompt may hold at most {MAX_SCHEDULE_TEXT_BYTES} bytes"
        )));
    }
    match spec {
        ScheduleSpec::Interval { every_secs } if *every_secs < MIN_SCHEDULE_INTERVAL_SECS => {
            return Err(refused(format!(
                "an interval must be at least {MIN_SCHEDULE_INTERVAL_SECS} seconds"
            )));
        }
        ScheduleSpec::Interval { every_secs } if *every_secs > MAX_SCHEDULE_INTERVAL_SECS => {
            return Err(refused(format!(
                "an interval must be at most {MAX_SCHEDULE_INTERVAL_SECS} seconds"
            )));
        }
        ScheduleSpec::Daily { minute_of_day, .. } if *minute_of_day >= MINUTES_PER_DAY => {
            return Err(refused("the minute of the day must be from 0 to 1439"));
        }
        _ => {}
    }
    if let ScheduleTarget::NewSession { cwd, .. } = target
        && !cwd.starts_with('/')
    {
        return Err(refused("the working directory must be an absolute path"));
    }
    Ok(())
}

fn read(row: &Row<'_>) -> Result<Result<ScheduleInfo, StoreError>, rusqlite::Error> {
    let kind: String = row.get(1)?;
    let spec = match kind.as_str() {
        "once" => ScheduleSpec::Once { at: row.get(2)? },
        "interval" => ScheduleSpec::Interval {
            every_secs: get_u64(row, 3)?,
        },
        _ => ScheduleSpec::Daily {
            minute_of_day: u32::try_from(get_u64(row, 4)?).unwrap_or(0),
            weekdays: weekdays_of(u8::try_from(get_u64(row, 5)?).unwrap_or(0)),
        },
    };
    let target_session: Option<i64> = row.get(6)?;
    let backend: Option<String> = row.get(7)?;
    let model: Option<String> = row.get(8)?;
    let cwd: Option<String> = row.get(9)?;
    let mode: Option<String> = row.get(10)?;
    let state: String = row.get(12)?;
    let info = ScheduleInfo {
        id: get_u64(row, 0)?,
        spec,
        target: ScheduleTarget::Session {
            session_id: session_id(0),
        },
        text: row.get(11)?,
        state: ScheduleState::Idle,
        next_run_at: row.get(13)?,
        last_run_at: row.get(14)?,
        runs: get_u64(row, 15)?,
        created_at: row.get(16)?,
        last_error: row.get(17)?,
        last_error_at: row.get(18)?,
    };
    Ok(decode(
        info,
        target_session,
        (backend, model, cwd, mode),
        state,
    ))
}

type NewSessionColumns = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn decode(
    mut info: ScheduleInfo,
    target_session: Option<i64>,
    (backend, model, cwd, mode): NewSessionColumns,
    state: String,
) -> Result<ScheduleInfo, StoreError> {
    info.state = decode_enum(state)?;
    info.target = match (target_session, backend, mode) {
        (Some(session), _, _) => ScheduleTarget::Session {
            session_id: session_id(session),
        },
        (None, Some(backend), Some(mode)) => ScheduleTarget::NewSession {
            backend: decode_enum(backend)?,
            model,
            cwd: cwd.unwrap_or_default(),
            mode: decode_enum(mode)?,
        },
        _ => return Err(refused("a stored schedule has no target")),
    };
    Ok(info)
}

impl Store {
    /// Adds a scheduled prompt.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ScheduleRefused`] when the prompt is empty or too long, an
    /// interval is under a minute, the time of day is out of range, a working directory is
    /// not absolute, or the store already keeps the most schedules it may, and
    /// [`StoreError::SessionNotFound`] when the target session does not exist.
    pub fn add_schedule(
        &mut self,
        spec: &ScheduleSpec,
        target: &ScheduleTarget,
        text: &str,
    ) -> Result<ScheduleInfo, StoreError> {
        check(spec, target, text)?;
        let target_row = match target {
            ScheduleTarget::Session { session_id } => {
                self.load_session(session_id)?;
                Some(
                    session_row_id(session_id).ok_or_else(|| StoreError::SessionNotFound {
                        session: session_id.clone(),
                    })?,
                )
            }
            ScheduleTarget::NewSession { .. } => None,
        };
        let now = self.clock.now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let live: i64 = tx.query_row(
            "SELECT COUNT(*) FROM scheduled_prompts WHERE state <> 'done'",
            [],
            |row| row.get(0),
        )?;
        if usize::try_from(live).unwrap_or(usize::MAX) >= MAX_SCHEDULES {
            return Err(refused(format!(
                "at most {MAX_SCHEDULES} scheduled prompts may exist; remove one first"
            )));
        }
        let (kind, at, every, minute, mask) = match spec {
            ScheduleSpec::Once { at } => ("once", Some(*at), None, None, 0),
            ScheduleSpec::Interval { every_secs } => (
                "interval",
                None,
                Some(i64::try_from(*every_secs).unwrap_or(i64::MAX)),
                None,
                0,
            ),
            ScheduleSpec::Daily {
                minute_of_day,
                weekdays,
            } => (
                "daily",
                None,
                None,
                Some(*minute_of_day),
                weekday_mask(weekdays),
            ),
        };
        let (backend, model, cwd, mode) = match target {
            ScheduleTarget::NewSession {
                backend,
                model,
                cwd,
                mode,
            } => (
                Some(encode_enum(backend)?),
                model.clone(),
                Some(cwd.clone()),
                Some(encode_enum(mode)?),
            ),
            ScheduleTarget::Session { .. } => (None, None, None, None),
        };
        tx.execute(
            "INSERT INTO scheduled_prompts (kind, at, every_secs, minute_of_day, weekdays, \
             target_session, backend, model, cwd, mode, text, state, next_run_at, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'idle', ?12, ?13)",
            params![
                kind,
                at,
                every,
                minute,
                mask,
                target_row,
                backend,
                model,
                cwd,
                mode,
                text,
                first_run(spec, now),
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        let added = tx.query_row(
            &format!("SELECT {COLUMNS} FROM scheduled_prompts WHERE id = ?1"),
            [id],
            read,
        )??;
        tx.commit()?;
        Ok(added)
    }

    /// Every scheduled prompt, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the query fails.
    pub fn list_schedules(&self) -> Result<Vec<ScheduleInfo>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM scheduled_prompts \
             ORDER BY state = 'done', id DESC LIMIT ?1"
        ))?;
        let limit = i64::try_from(MAX_LISTED_SCHEDULES).unwrap_or(i64::MAX);
        let rows = stmt.query_map([limit], read)?;
        let mut listed = rows.map(|row| row?).collect::<Result<Vec<_>, _>>()?;
        listed.sort_by_key(|schedule| schedule.id);
        Ok(listed)
    }

    /// Removes a scheduled prompt; returns whether one had that id.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the statement fails.
    pub fn remove_schedule(&mut self, id: u64) -> Result<bool, StoreError> {
        let id = i64::try_from(id).unwrap_or(i64::MAX);
        let removed = self
            .conn
            .execute("DELETE FROM scheduled_prompts WHERE id = ?1", [id])?;
        Ok(removed == 1)
    }

    /// Claims up to `limit` prompts that are due now: each is set running and its next run
    /// moved forward, all in one transaction. A prompt that is running is never claimed
    /// again until [`Store::finish_schedule_run`].
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the transaction fails.
    pub fn claim_due_schedules(&mut self, limit: usize) -> Result<Vec<ScheduleInfo>, StoreError> {
        let now = self.clock.now_ms();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let due: Vec<ScheduleInfo> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {COLUMNS} FROM scheduled_prompts \
                 WHERE state = 'idle' AND next_run_at IS NOT NULL AND next_run_at <= ?1 \
                 ORDER BY next_run_at, id LIMIT ?2"
            ))?;
            let rows = stmt.query_map(params![now, limit], read)?;
            rows.map(|row| row?).collect::<Result<_, _>>()?
        };
        let mut claimed = Vec::with_capacity(due.len());
        for mut schedule in due {
            let next = next_after(&schedule.spec, now);
            tx.execute(
                "UPDATE scheduled_prompts SET state = 'running', next_run_at = ?1, \
                 last_run_at = ?2, runs = runs + 1, last_error = NULL, last_error_at = NULL \
                 WHERE id = ?3",
                params![next, now, i64::try_from(schedule.id).unwrap_or(i64::MAX)],
            )?;
            schedule.state = ScheduleState::Running;
            schedule.next_run_at = next;
            schedule.last_run_at = Some(now);
            schedule.runs += 1;
            schedule.last_error = None;
            schedule.last_error_at = None;
            claimed.push(schedule);
        }
        tx.commit()?;
        Ok(claimed)
    }

    /// Ends a run: the prompt waits for its next run, or is done when it has none.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the statement fails.
    pub fn finish_schedule_run(&mut self, id: u64) -> Result<(), StoreError> {
        let id = i64::try_from(id).unwrap_or(i64::MAX);
        self.conn.execute(
            "UPDATE scheduled_prompts SET state = \
             CASE WHEN next_run_at IS NULL THEN 'done' ELSE 'idle' END \
             WHERE id = ?1 AND state = 'running'",
            [id],
        )?;
        Ok(())
    }

    /// Records why the run of schedule `id` failed, with the time, cut to
    /// [`MAX_SCHEDULE_ERROR_CHARS`] characters.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the statement fails.
    pub fn record_schedule_failure(&mut self, id: u64, reason: &str) -> Result<(), StoreError> {
        let id = i64::try_from(id).unwrap_or(i64::MAX);
        let reason: String = reason
            .chars()
            .take(MAX_SCHEDULE_ERROR_CHARS)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let now = self.clock.now_ms();
        self.conn.execute(
            "UPDATE scheduled_prompts SET last_error = ?1, last_error_at = ?2 WHERE id = ?3",
            params![reason, now, id],
        )?;
        Ok(())
    }

    /// Ends a schedule for good: it gets no next run, and is done once its current run ends.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the statement fails.
    pub fn stop_schedule(&mut self, id: u64) -> Result<(), StoreError> {
        let id = i64::try_from(id).unwrap_or(i64::MAX);
        self.conn.execute(
            "UPDATE scheduled_prompts SET next_run_at = NULL WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    /// Deletes the done schedules whose last run, or creation when they never ran, is
    /// more than `retention_ms` ago. Returns how many it deleted.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the statement fails.
    pub fn prune_done_schedules(&mut self, retention_ms: i64) -> Result<usize, StoreError> {
        let cutoff = self.clock.now_ms().saturating_sub(retention_ms);
        Ok(self.conn.execute(
            "DELETE FROM scheduled_prompts \
             WHERE state = 'done' AND COALESCE(last_run_at, created_at) < ?1",
            [cutoff],
        )?)
    }

    /// Puts back the prompts a crash left running: a repeating one waits for its next run,
    /// and a one-time one runs again at the next tick, since nothing says it ran. Returns how
    /// many it found.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] when the statement fails.
    pub fn recover_schedules(&mut self) -> Result<usize, StoreError> {
        let now = self.clock.now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE scheduled_prompts SET next_run_at = ?1, runs = MAX(runs - 1, 0) \
             WHERE state = 'running' AND next_run_at IS NULL",
            [now],
        )?;
        let found = tx.execute(
            "UPDATE scheduled_prompts SET state = 'idle' WHERE state = 'running'",
            [],
        )?;
        tx.commit()?;
        Ok(found)
    }
}
