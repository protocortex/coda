// SPDX-License-Identifier: Apache-2.0

//! What `coda review` remembers about a pull request: the head commit of the last run and the
//! findings it reported, kept in the store. A later run feeds them to the reviewer, hides what
//! the user dismissed, and with `--incremental` reviews only what came after the last run.
//! The memory is an aid: when it cannot be opened the review goes on without it.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use coda_config::review::ReviewConfig;
use coda_review::diff::{FileDiff, git_diff};
use coda_review::memory::{
    LastRun, Remembered, Scope, Thresholds, code_hash, drop_dismissed, incremental_scope,
    prompt_section, resolved,
};
use coda_review::parse::Parsed;
use coda_review::post::fingerprint;
use coda_review::request::Target;
use coda_review::schema::{Finding, Review};
use coda_store::{FindingStatus, MemoryFinding, Store, StoredFinding};

use crate::context::{Context, Io};
use crate::error::CliError;
use crate::review_post::{RealGh, identify};

const GIT: &str = "git";

/// The store and the pull request this run is about.
pub struct Memory {
    store: Store,
    key: String,
    head: String,
    stored: Vec<StoredFinding>,
}

fn remembered(stored: &StoredFinding) -> Remembered {
    let f = &stored.finding;
    Remembered {
        fingerprint: f.fingerprint.clone(),
        file: f.file.clone(),
        start_line: f.start_line,
        end_line: f.end_line,
        title: f.title.clone(),
        explanation: f.explanation.clone(),
        code_hash: f.code_hash.clone(),
    }
}

fn now_ms() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

impl Memory {
    /// Opens the memory of the pull request `target` names. A branch, a range or a staged
    /// change has none, and neither does a pull request when the store cannot be opened:
    /// that is a note, not an error, unless `needed` says `--incremental` was asked for.
    ///
    /// # Errors
    /// Returns [`CliError::ReviewMemory`] when `needed` and there is no memory to read.
    pub fn open(
        ctx: &Context,
        repo: &Path,
        target: Option<&Target>,
        io: &mut Io<'_>,
        needed: bool,
    ) -> Result<Option<Self>, CliError> {
        let Some(Target::PullRequest(pr)) = target else {
            return if needed {
                Err(CliError::ReviewMemory(
                    "only a pull request is remembered; name one by number or URL".to_owned(),
                ))
            } else {
                Ok(None)
            };
        };
        let opened = identify(&mut RealGh { repo }, pr).and_then(|(key, head)| {
            let store = Store::open(&ctx.dirs.data_dir())
                .map_err(|e| CliError::ReviewMemory(e.to_string()))?;
            Self::new(store, key, head)
        });
        match opened {
            Ok(memory) => Ok(Some(memory)),
            Err(error) if needed => Err(error),
            Err(error) => {
                writeln!(io.err, "coda: no review memory this run: {error}")?;
                Ok(None)
            }
        }
    }

    fn new(store: Store, key: String, head: String) -> Result<Self, CliError> {
        let stored = store
            .review_findings(&key)
            .map_err(|e| CliError::ReviewMemory(e.to_string()))?;
        Ok(Self {
            store,
            key,
            head,
            stored,
        })
    }

    /// What this run reviews: everything, or with `incremental` only what came after the last
    /// run, or nothing.
    pub fn scope(&self, incremental: bool, repo: &Path, config: &ReviewConfig) -> Scope {
        if !incremental {
            return Scope::Full;
        }
        let last = self.store.last_review_run(&self.key).ok().flatten();
        let last = last.map(|run| LastRun {
            head_sha: run.head_sha,
            at_ms: run.created_at_ms,
        });
        let commits = last
            .as_ref()
            .and_then(|run| commits_between(repo, &run.head_sha, &self.head));
        incremental_scope(
            last.as_ref(),
            &self.head,
            commits,
            now_ms(),
            Thresholds {
                min_commits: config.incremental_min_commits,
                min_minutes: config.incremental_min_minutes,
            },
        )
    }

    /// The diff of what came after `since`, up to the head of the pull request.
    ///
    /// # Errors
    /// Returns the git error when `since` is not in the repository, such as after a force
    /// push: the caller then reviews everything.
    pub fn diff_since(&self, repo: &Path, since: &str) -> Result<String, CliError> {
        git_diff(repo, since, &self.head).map_err(|e| CliError::ReviewMemory(e.to_string()))
    }

    /// The text for the reviewer's prompt: what earlier runs found, and what the user
    /// dismissed.
    #[must_use]
    pub fn prompt(&self) -> String {
        let pick = |status| {
            self.stored
                .iter()
                .filter(|s| s.status == status)
                .map(remembered)
                .collect::<Vec<_>>()
        };
        prompt_section(&pick(FindingStatus::Open), &pick(FindingStatus::Dismissed))
    }

    /// Removes from `parsed` the findings the user dismissed, whatever words the reviewer
    /// used for them this time.
    pub fn drop_dismissed(&self, parsed: &mut Parsed) {
        let dismissed: BTreeSet<String> = self
            .stored
            .iter()
            .filter(|s| s.status == FindingStatus::Dismissed)
            .map(|s| s.finding.fingerprint.clone())
            .collect();
        let findings = std::mem::take(&mut parsed.findings);
        parsed.findings = drop_dismissed(findings, &dismissed).0;
    }

    /// Records the run. `files` is the diff this run reviewed. Only a run of the whole change
    /// that finished without a failed part may resolve findings: an incremental or partial run
    /// did not see everything, so a finding it does not report is not gone.
    ///
    /// # Errors
    /// Returns [`CliError::ReviewMemory`] when the store refuses the write.
    pub fn record(
        &mut self,
        findings: &[Finding],
        files: &[FileDiff],
        sees_everything: bool,
    ) -> Result<(), CliError> {
        let reported: Vec<MemoryFinding> = findings
            .iter()
            .map(|f| MemoryFinding {
                fingerprint: fingerprint(f),
                file: f.file.clone(),
                start_line: f.start_line,
                end_line: f.end_line,
                severity: f.severity.as_str().to_owned(),
                title: f.title.clone(),
                explanation: f.explanation.clone(),
                code_hash: code_hash(files, &f.file, f.start_line, f.end_line).unwrap_or_default(),
            })
            .collect();
        let gone = if sees_everything {
            let open: Vec<Remembered> = self
                .stored
                .iter()
                .filter(|s| s.status == FindingStatus::Open)
                .map(remembered)
                .collect();
            let names: BTreeSet<String> = reported.iter().map(|r| r.fingerprint.clone()).collect();
            resolved(&open, &names, files)
        } else {
            Vec::new()
        };
        self.store
            .record_review_run(&self.key, &self.head, sees_everything, &reported, &gone)
            .map_err(|e| CliError::ReviewMemory(e.to_string()))
    }

    /// Dismisses the finding each of `prefixes` names, and says what happened to each.
    ///
    /// # Errors
    /// Returns [`CliError::ReviewMemory`] when a prefix matches no finding or more than one,
    /// after the others were dismissed.
    pub fn dismiss(&mut self, prefixes: &[String], io: &mut Io<'_>) -> Result<(), CliError> {
        let mut problems = Vec::new();
        for prefix in prefixes {
            let matched = self
                .store
                .dismiss_finding(&self.key, prefix)
                .map_err(|e| CliError::ReviewMemory(e.to_string()))?;
            match matched {
                1 => writeln!(io.err, "coda: dismissed {prefix}")?,
                0 => problems.push(format!("no finding of {} starts with {prefix}", self.key)),
                n => problems.push(format!(
                    "{prefix} matches {n} findings; give more characters of the fingerprint"
                )),
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(CliError::ReviewMemory(problems.join("; ")))
        }
    }
}

/// Whether a run saw the whole change: it read all of it, no part failed, and no file was
/// clipped or left out. Only such a run may resolve findings.
pub fn saw_everything(review: &Review, read_whole_diff: bool) -> bool {
    read_whole_diff
        && !review.run.partial
        && review.coverage.clipped.is_empty()
        && review.coverage.left_out.is_empty()
}

/// How many commits `head` has that `last` does not. `None` when git cannot say, such as when
/// `last` is not in the repository.
fn commits_between(repo: &Path, last: &str, head: &str) -> Option<usize> {
    let range = format!("{last}..{head}");
    let out = Command::new(GIT)
        .args(["rev-list", "--count", &range])
        .current_dir(repo)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use coda_review::diff::parse_diff;
    use coda_review::schema::{Category, Confidence, Severity};
    use tempfile::TempDir;

    use super::*;

    const KEY: &str = "acme/widgets#7";
    const DIFF: &str =
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,3 @@\n one\n+two\n three\n";

    fn memory(dir: &TempDir, head: &str) -> Memory {
        let store = Store::open(&dir.path().join("data")).unwrap();
        Memory::new(store, KEY.to_owned(), head.to_owned()).unwrap()
    }

    fn finding(title: &str) -> Finding {
        Finding {
            id: "F1".to_owned(),
            file: "a.rs".to_owned(),
            start_line: 2,
            end_line: 2,
            severity: Severity::High,
            category: Category::Bug,
            title: title.to_owned(),
            explanation: "because".to_owned(),
            suggested_patch: None,
            confidence: Confidence::High,
            score: None,
            verifier_reason: None,
            unverified: false,
        }
    }

    fn no_io() -> (Vec<u8>, Vec<u8>, std::io::Cursor<Vec<u8>>) {
        (Vec::new(), Vec::new(), std::io::Cursor::new(Vec::new()))
    }

    #[test]
    fn a_dismissed_finding_is_hidden_from_the_next_run_even_reworded() {
        // Arrange
        let dir = TempDir::new().unwrap();
        let mut first = memory(&dir, "sha1");
        first
            .record(&[finding("Off by one")], &parse_diff(DIFF), true)
            .unwrap();
        let print = fingerprint(&finding("Off by one"));
        let (mut out, mut err, mut input) = no_io();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            input: &mut input,
        };
        first.dismiss(&[print[..6].to_owned()], &mut io).unwrap();
        drop(first);

        // Act
        let second = memory(&dir, "sha2");
        let mut parsed = Parsed {
            summary: String::new(),
            effort: 1,
            risk: coda_review::schema::Risk::Low,
            findings: vec![finding("off BY one!"), finding("Something else")],
            rejected: vec![],
            over_cap: 0,
            compliance: vec![],
        };
        second.drop_dismissed(&mut parsed);

        // Assert
        assert_eq!(parsed.findings.len(), 1);
        assert_eq!(parsed.findings[0].title, "Something else");
        assert!(second.prompt().contains("Do not report these again"));
    }

    #[test]
    fn a_full_run_resolves_a_finding_whose_code_changed_and_a_partial_run_does_not() {
        // Arrange
        let dir = TempDir::new().unwrap();
        let mut first = memory(&dir, "sha1");
        first
            .record(&[finding("Off by one")], &parse_diff(DIFF), true)
            .unwrap();
        let edited = parse_diff(&DIFF.replace("+two", "+TWO"));

        // Act
        let mut partial = memory(&dir, "sha2");
        partial.record(&[], &edited, false).unwrap();
        let after_partial = memory(&dir, "sha2").stored;
        let mut full = memory(&dir, "sha3");
        full.record(&[], &edited, true).unwrap();
        let after_full = memory(&dir, "sha3").stored;

        // Assert
        assert_eq!(after_partial[0].status, FindingStatus::Open);
        assert_eq!(after_full[0].status, FindingStatus::Resolved);
    }

    #[test]
    fn a_run_that_left_files_out_or_clipped_them_does_not_see_everything() {
        // Arrange
        let mut review = Review {
            summary: String::new(),
            effort: 1,
            risk: coda_review::schema::Risk::Low,
            findings: vec![],
            compliance: vec![],
            coverage: coda_review::schema::Coverage::default(),
            run: coda_review::schema::RunDetails::default(),
        };
        let whole = saw_everything(&review, true);
        review.coverage.clipped = vec!["big.rs".to_owned()];
        let clipped = saw_everything(&review, true);
        review.coverage.clipped.clear();
        review.coverage.left_out = vec!["late.rs".to_owned()];
        let left_out = saw_everything(&review, true);

        // Assert
        assert!(whole);
        assert!(!clipped);
        assert!(!left_out);
        assert!(!saw_everything(&review_with_partial(), true));
        assert!(!saw_everything(&review, false));
    }

    fn review_with_partial() -> Review {
        let mut review = Review {
            summary: String::new(),
            effort: 1,
            risk: coda_review::schema::Risk::Low,
            findings: vec![],
            compliance: vec![],
            coverage: coda_review::schema::Coverage::default(),
            run: coda_review::schema::RunDetails::default(),
        };
        review.run.partial = true;
        review
    }

    #[test]
    fn every_bad_prefix_is_named_in_the_error() {
        // Arrange
        let dir = TempDir::new().unwrap();
        let mut memory = memory(&dir, "sha1");
        let (mut out, mut err, mut input) = no_io();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            input: &mut input,
        };

        // Act
        let result = memory.dismiss(&["zz".to_owned(), "yy".to_owned()], &mut io);

        // Assert
        assert!(
            matches!(result, Err(CliError::ReviewMemory(why)) if why.contains("zz") && why.contains("yy"))
        );
    }

    #[test]
    fn an_unknown_or_ambiguous_prefix_is_an_error() {
        // Arrange
        let dir = TempDir::new().unwrap();
        let mut memory = memory(&dir, "sha1");
        let (mut out, mut err, mut input) = no_io();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            input: &mut input,
        };

        // Act
        let result = memory.dismiss(&["zz".to_owned()], &mut io);

        // Assert
        assert!(matches!(result, Err(CliError::ReviewMemory(why)) if why.contains("zz")));
    }

    #[test]
    fn the_first_incremental_run_reviews_everything_and_an_unchanged_head_does_nothing() {
        // Arrange
        let dir = TempDir::new().unwrap();
        let mut memory = memory(&dir, "sha1");
        let config = ReviewConfig::default();
        let repo = dir.path();

        // Act
        let first = memory.scope(true, repo, &config);
        memory.record(&[], &parse_diff(DIFF), true).unwrap();
        let again = memory.scope(true, repo, &config);
        let plain = memory.scope(false, repo, &config);

        // Assert
        assert_eq!(first, Scope::Full);
        assert!(matches!(again, Scope::Skip(why) if why.contains("already reviewed")));
        assert_eq!(plain, Scope::Full);
    }
}
