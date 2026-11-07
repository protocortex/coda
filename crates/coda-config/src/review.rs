// SPDX-License-Identifier: Apache-2.0

//! The `[review]` table: limits for `coda review`.

use serde::Deserialize;

/// Key of the table in `config.toml`.
pub const REVIEW_KEY: &str = "review";
/// Key of the finding cap inside `[review]`.
pub const MAX_FINDINGS_KEY: &str = "max_findings";
/// Findings kept per run when the file does not say.
pub const DEFAULT_MAX_FINDINGS: usize = 5;

/// Key of the diff size budget inside `[review]`.
pub const DIFF_BUDGET_CHARS_KEY: &str = "diff_budget_chars";
/// Characters of diff text sent to the model when the file does not say.
pub const DEFAULT_DIFF_BUDGET_CHARS: usize = 120_000;

/// Key of the chunk cap inside `[review]`.
pub const MAX_CHUNKS_KEY: &str = "max_chunks";
/// Most chunks one review is split into when the file does not say.
pub const DEFAULT_MAX_CHUNKS: usize = 4;

/// Key of the chunk parallelism inside `[review]`.
pub const CHUNK_CONCURRENCY_KEY: &str = "chunk_concurrency";
/// Chunks reviewed at the same time when the file does not say.
pub const DEFAULT_CHUNK_CONCURRENCY: usize = 3;

/// Key of the verifier switch inside `[review]`.
pub const VERIFY_KEY: &str = "verify";
/// Key of the verifier score floor inside `[review]`.
pub const VERIFY_THRESHOLD_KEY: &str = "verify_threshold";
/// Findings the verifier scores below this are dropped when the file does not say.
pub const DEFAULT_VERIFY_THRESHOLD: u8 = 4;
/// Highest score the verifier gives.
pub const VERIFY_THRESHOLD_MAX: u8 = 10;

/// Lines of the repository's guidance files that go in the reviewer's prompt when the file
/// does not say.
pub const DEFAULT_GUIDANCE_LINES: usize = 500;

/// The `[review]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ReviewConfig {
    /// Most findings one review run reports. Must not be zero.
    pub max_findings: usize,
    /// Most characters of diff text one review sends to the model. Must not be zero.
    pub diff_budget_chars: usize,
    /// Most parts one review splits a change into when it does not fit one run. Files past
    /// the last part are left out. Must not be zero.
    pub max_chunks: usize,
    /// Most parts reviewed at the same time. Must not be zero.
    pub chunk_concurrency: usize,
    /// Whether a second agent checks the findings and drops the ones it doubts.
    pub verify: bool,
    /// Findings the verifier scores below this, from 0 to 10, are dropped. A score of 0 is
    /// always dropped.
    pub verify_threshold: u8,
    /// Most lines of the repository's `AGENTS.md`, `CLAUDE.md` and `docs/adr/*.md`, read from
    /// the default branch, that go in the reviewer's prompt as project rules. Zero leaves
    /// them out.
    pub guidance_lines: usize,
    /// Instructions added to every review's prompt, after the project rules.
    pub extra_instructions: String,
    /// Files left out of a review.
    pub ignore: ReviewIgnore,
    /// Pull requests that are not reviewed.
    pub skip: ReviewSkip,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            max_findings: DEFAULT_MAX_FINDINGS,
            diff_budget_chars: DEFAULT_DIFF_BUDGET_CHARS,
            max_chunks: DEFAULT_MAX_CHUNKS,
            chunk_concurrency: DEFAULT_CHUNK_CONCURRENCY,
            verify: true,
            verify_threshold: DEFAULT_VERIFY_THRESHOLD,
            guidance_lines: DEFAULT_GUIDANCE_LINES,
            extra_instructions: String::new(),
            ignore: ReviewIgnore::default(),
            skip: ReviewSkip::default(),
        }
    }
}

/// The `[review.ignore]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ReviewIgnore {
    /// Globs of files to leave out. A glob with no `/` matches the file name at any depth; a
    /// leading `/` pins it to the repository root.
    pub globs: Vec<String>,
    /// Whether the built-in list of generated files (lockfiles, minified files, source
    /// maps, generated protobuf code) is also left out.
    pub builtin: bool,
}

impl Default for ReviewIgnore {
    fn default() -> Self {
        Self {
            globs: Vec::new(),
            builtin: true,
        }
    }
}

/// The `[review.skip]` section. Each rule is off until set; one match skips the pull request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ReviewSkip {
    /// Skip a draft pull request.
    pub draft: bool,
    /// Skip a pull request that has one of these labels (case-insensitive).
    pub labels: Vec<String>,
    /// Skip when the title matches one of these regular expressions.
    pub title_regex: Vec<String>,
    /// Skip when the source branch matches one of these regular expressions.
    pub source_branch_regex: Vec<String>,
    /// Skip when the target branch matches one of these regular expressions.
    pub target_branch_regex: Vec<String>,
    /// Skip when the author is one of these logins (case-insensitive).
    pub authors: Vec<String>,
    /// Skip when the author is a bot.
    pub bots: bool,
}
