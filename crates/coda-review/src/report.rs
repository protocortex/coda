// SPDX-License-Identifier: Apache-2.0

//! The report of one review run: how it went, with no diff text, for the terminal and for the
//! JSONL file under the data directory.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::schema::Review;

const MS_PER_SECOND: u64 = 1000;
const MS_PER_TENTH: u64 = 100;

/// Name of the JSONL file in the data directory.
pub const REPORT_FILE_NAME: &str = "review-runs.jsonl";

/// Tokens the backends reported for a review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    /// Tokens sent to the models.
    pub input: u64,
    /// Tokens the models produced.
    pub output: u64,
}

/// How one review run went. It holds counts and names only: no diff, finding or file text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    /// When the run ended, in seconds since the Unix epoch.
    pub at: u64,
    /// The backend that was asked to review.
    pub backend: String,
    /// The model that answered.
    pub model: String,
    /// Wall time of the whole run, in milliseconds.
    pub wall_ms: u64,
    /// Parts the change was split into.
    pub chunks: usize,
    /// Whether a part failed, so the review is partial.
    pub partial: bool,
    /// One line for each part that moved to another backend. Empty when none did.
    pub fallbacks: Vec<String>,
    /// The backend the verifier ran on, when it ran.
    pub verifier: Option<String>,
    /// Findings in the review.
    pub findings: usize,
    /// Context files added to the prompt.
    pub context_files: usize,
    /// Whether the configured checks were run first.
    pub checks_run: bool,
    /// Tokens, only when a backend reported them.
    pub tokens: Option<Tokens>,
}

/// What a run adds to its report besides the review itself.
#[derive(Debug, Clone)]
pub struct RunFacts {
    /// When the run ended, in seconds since the Unix epoch.
    pub at: u64,
    /// The backend that was asked to review.
    pub backend: String,
    /// Wall time of the whole run, in milliseconds.
    pub wall_ms: u64,
    /// Context files added to the prompt.
    pub context_files: usize,
    /// Whether the configured checks were run first.
    pub checks_run: bool,
    /// Tokens, only when a backend reported them.
    pub tokens: Option<Tokens>,
}

impl RunReport {
    /// The report of `review`.
    #[must_use]
    pub fn of(review: &Review, facts: RunFacts) -> Self {
        Self {
            at: facts.at,
            backend: facts.backend,
            model: review.run.model.clone(),
            wall_ms: facts.wall_ms,
            chunks: review.run.chunks,
            partial: review.run.partial,
            fallbacks: review.run.fallbacks.clone(),
            verifier: review.run.verifier.clone(),
            findings: review.findings.len(),
            context_files: facts.context_files,
            checks_run: facts.checks_run,
            tokens: facts.tokens,
        }
    }

    /// The report as lines for a person.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = format!(
            "Review run: backend {}, model {}, {}.{} s, {} chunk{}",
            self.backend,
            self.model,
            self.wall_ms / MS_PER_SECOND,
            self.wall_ms % MS_PER_SECOND / MS_PER_TENTH,
            self.chunks,
            if self.chunks == 1 { "" } else { "s" },
        );
        let _ = write!(
            out,
            ", fallback {}, verifier {}",
            if self.fallbacks.is_empty() {
                "not used"
            } else {
                "used"
            },
            self.verifier.as_deref().unwrap_or("not used"),
        );
        if let Some(tokens) = self.tokens {
            let _ = write!(
                out,
                ", tokens {} in and {} out",
                tokens.input, tokens.output
            );
        }
        out.push('\n');
        for note in &self.fallbacks {
            let _ = writeln!(out, "  {note}");
        }
        out
    }

    /// The report as one line of JSON, with its line break.
    ///
    /// # Errors
    /// Returns the serializer's error, which a report of plain fields never gives.
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }
}

/// Adds `report` as a line to the report file in `dir`, which it makes when needed.
///
/// # Errors
/// Returns the error of making the directory or writing the file.
pub fn append(dir: &Path, report: &RunReport) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(REPORT_FILE_NAME);
    let line = report.to_line().map_err(io::Error::other)?;
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    file.write_all(line.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Category, Confidence, Coverage, Finding, Risk, RunDetails, Severity};

    const SECRET_DIFF_LINE: &str = "let password = \"hunter2\";";

    fn review() -> Review {
        let finding = Finding {
            id: "F1".to_owned(),
            file: "src/secret.rs".to_owned(),
            start_line: 1,
            end_line: 1,
            severity: Severity::High,
            category: Category::Bug,
            title: SECRET_DIFF_LINE.to_owned(),
            explanation: SECRET_DIFF_LINE.to_owned(),
            suggested_patch: Some(SECRET_DIFF_LINE.to_owned()),
            confidence: Confidence::High,
            score: None,
            verifier_reason: None,
            unverified: false,
        };
        Review {
            summary: SECRET_DIFF_LINE.to_owned(),
            effort: 2,
            risk: Risk::Low,
            findings: vec![finding],
            compliance: vec![],
            coverage: Coverage::default(),
            run: RunDetails {
                model: "m1".to_owned(),
                input_chars: 10,
                dropped_findings: 0,
                verifier: Some("codex_app_server".to_owned()),
                verifier_dropped: 0,
                chunks: 3,
                partial: false,
                skipped_issues: vec![],
                fallbacks: vec![
                    "part 2 of 3: claude_code hit a usage limit, so it ran on codex_app_server"
                        .to_owned(),
                ],
            },
        }
    }

    fn facts(tokens: Option<Tokens>) -> RunFacts {
        RunFacts {
            at: 1_700_000_000,
            backend: "claude_code".to_owned(),
            wall_ms: 12_340,
            context_files: 2,
            checks_run: true,
            tokens,
        }
    }

    #[test]
    fn the_line_reads_back_as_the_same_report_and_holds_no_diff_text() {
        // Arrange
        let report = RunReport::of(
            &review(),
            facts(Some(Tokens {
                input: 900,
                output: 80,
            })),
        );

        // Act
        let line = report.to_line().unwrap();
        let back: RunReport = serde_json::from_str(line.trim_end()).unwrap();

        // Assert
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        assert_eq!(back, report);
        assert!(!line.contains("hunter2"), "{line}");
        assert!(!line.contains("src/secret.rs"), "{line}");
        assert!(!report.text().contains("hunter2"));
    }

    #[test]
    fn tokens_are_in_the_report_only_when_a_backend_gave_them() {
        // Arrange
        let with = RunReport::of(
            &review(),
            facts(Some(Tokens {
                input: 900,
                output: 80,
            })),
        );
        let without = RunReport::of(&review(), facts(None));

        // Act
        let with_json: serde_json::Value = serde_json::from_str(&with.to_line().unwrap()).unwrap();
        let without_json: serde_json::Value =
            serde_json::from_str(&without.to_line().unwrap()).unwrap();

        // Assert
        assert_eq!(with_json["tokens"]["input"], 900);
        assert!(without_json["tokens"].is_null());
        assert!(with.text().contains("tokens 900 in and 80 out"));
        assert!(!without.text().contains("tokens"));
    }

    #[test]
    fn the_text_names_backend_model_time_chunks_fallback_and_verifier() {
        // Arrange
        let report = RunReport::of(&review(), facts(None));

        // Act
        let text = report.text();

        // Assert
        assert!(text.starts_with(
            "Review run: backend claude_code, model m1, 12.3 s, 3 chunks, fallback used, verifier codex_app_server\n"
        ), "{text}");
        assert!(text.contains("part 2 of 3: claude_code hit a usage limit"));
        let mut plain = review();
        plain.run.fallbacks.clear();
        plain.run.verifier = None;
        plain.run.chunks = 1;
        assert!(
            RunReport::of(&plain, facts(None))
                .text()
                .contains("1 chunk, fallback not used, verifier not used")
        );
    }

    #[test]
    fn each_run_adds_a_line_and_keeps_the_earlier_ones() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("data");
        let report = RunReport::of(&review(), facts(None));

        // Act
        append(&nested, &report).unwrap();
        let path = append(&nested, &report).unwrap();

        // Assert
        let text = fs::read_to_string(path).unwrap();
        assert_eq!(text.lines().count(), 2);
        for line in text.lines() {
            assert_eq!(serde_json::from_str::<RunReport>(line).unwrap(), report);
        }
    }
}
