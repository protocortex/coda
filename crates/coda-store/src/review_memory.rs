// SPDX-License-Identifier: Apache-2.0

//! What `coda review` remembers about a pull request: the head commit of each run and the
//! findings it reported, so a later run can skip what was dismissed and review only what is
//! new.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::{Store, StoreError};

const STATUS_OPEN: &str = "open";
const STATUS_RESOLVED: &str = "resolved";
const STATUS_DISMISSED: &str = "dismissed";

/// Where a finding stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingStatus {
    /// Reported and not dealt with.
    Open,
    /// The code it pointed at changed and the reviewer no longer reports it.
    Resolved,
    /// The user said it is not a problem. It stays so when the reviewer reports it again.
    Dismissed,
}

impl FindingStatus {
    fn parse(text: &str) -> Self {
        match text {
            STATUS_RESOLVED => Self::Resolved,
            STATUS_DISMISSED => Self::Dismissed,
            _ => Self::Open,
        }
    }
}

/// A finding as a run reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryFinding {
    /// Stable id of the finding across runs.
    pub fingerprint: String,
    /// Path of the file.
    pub file: String,
    /// First line of the range, on the new side.
    pub start_line: u32,
    /// Last line of the range.
    pub end_line: u32,
    /// How serious it is, as the review writes it.
    pub severity: String,
    /// One-line headline.
    pub title: String,
    /// Why it matters.
    pub explanation: String,
    /// Hash of the code lines the finding points at.
    pub code_hash: String,
}

/// A finding and where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFinding {
    /// The finding as last reported.
    pub finding: MemoryFinding,
    /// Where it stands.
    pub status: FindingStatus,
}

/// A recorded run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRunRecord {
    /// The head commit the run reviewed.
    pub head_sha: String,
    /// True when the run reviewed the whole change, false for an incremental one.
    pub full: bool,
    /// When it was recorded, in milliseconds since the Unix epoch.
    pub created_at_ms: i64,
}

impl Store {
    /// The latest run recorded for `pr_key`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`].
    pub fn last_review_run(&self, pr_key: &str) -> Result<Option<ReviewRunRecord>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT head_sha, full, created_at FROM review_runs \
                 WHERE pr_key = ?1 ORDER BY id DESC LIMIT 1",
                [pr_key],
                |row| {
                    Ok(ReviewRunRecord {
                        head_sha: row.get(0)?,
                        full: row.get::<_, i64>(1)? == 1,
                        created_at_ms: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Every finding remembered for `pr_key`, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`].
    pub fn review_findings(&self, pr_key: &str) -> Result<Vec<StoredFinding>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT fingerprint, file, start_line, end_line, severity, title, explanation, \
             code_hash, status FROM review_findings WHERE pr_key = ?1 ORDER BY first_run, rowid",
        )?;
        let rows = stmt.query_map([pr_key], |row| {
            Ok(StoredFinding {
                finding: MemoryFinding {
                    fingerprint: row.get(0)?,
                    file: row.get(1)?,
                    start_line: row.get(2)?,
                    end_line: row.get(3)?,
                    severity: row.get(4)?,
                    title: row.get(5)?,
                    explanation: row.get(6)?,
                    code_hash: row.get(7)?,
                },
                status: FindingStatus::parse(&row.get::<_, String>(8)?),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records a run of `pr_key` on `head_sha`. Each reported finding is stored as open, with
    /// its newest text and code hash, unless the user dismissed it: that stays. Each
    /// fingerprint in `resolved` that is open becomes resolved; a partial run passes none.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`]; nothing is recorded then.
    pub fn record_review_run(
        &mut self,
        pr_key: &str,
        head_sha: &str,
        full: bool,
        reported: &[MemoryFinding],
        resolved: &[String],
    ) -> Result<(), StoreError> {
        let now = self.clock.now_ms();
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO review_runs (pr_key, head_sha, full, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![pr_key, head_sha, i64::from(full), now],
        )?;
        let run = tx.last_insert_rowid();
        for f in reported {
            tx.execute(
                "INSERT INTO review_findings (pr_key, fingerprint, file, start_line, end_line, \
                 severity, title, explanation, code_hash, status, first_run, last_run) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11) \
                 ON CONFLICT (pr_key, fingerprint) DO UPDATE SET \
                 file = excluded.file, start_line = excluded.start_line, \
                 end_line = excluded.end_line, severity = excluded.severity, \
                 title = excluded.title, explanation = excluded.explanation, \
                 code_hash = excluded.code_hash, last_run = excluded.last_run, \
                 status = CASE WHEN review_findings.status = ?12 THEN ?12 ELSE ?10 END",
                params![
                    pr_key,
                    f.fingerprint,
                    f.file,
                    f.start_line,
                    f.end_line,
                    f.severity,
                    f.title,
                    f.explanation,
                    f.code_hash,
                    STATUS_OPEN,
                    run,
                    STATUS_DISMISSED,
                ],
            )?;
        }
        for fingerprint in resolved {
            tx.execute(
                "UPDATE review_findings SET status = ?1 \
                 WHERE pr_key = ?2 AND fingerprint = ?3 AND status = ?4",
                params![STATUS_RESOLVED, pr_key, fingerprint, STATUS_OPEN],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Marks the finding of `pr_key` whose fingerprint starts with `prefix` as dismissed.
    /// Returns how many findings matched; when it is not exactly one, nothing is changed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`].
    pub fn dismiss_finding(&mut self, pr_key: &str, prefix: &str) -> Result<usize, StoreError> {
        if prefix.is_empty() {
            return Ok(0);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let matched: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT fingerprint FROM review_findings \
                 WHERE pr_key = ?1 AND substr(fingerprint, 1, length(?2)) = ?2",
            )?;
            let rows = stmt.query_map(params![pr_key, prefix], |row| row.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        if let [only] = matched.as_slice() {
            tx.execute(
                "UPDATE review_findings SET status = ?1 WHERE pr_key = ?2 AND fingerprint = ?3",
                params![STATUS_DISMISSED, pr_key, only],
            )?;
        }
        tx.commit()?;
        Ok(matched.len())
    }
}
