// SPDX-License-Identifier: Apache-2.0

//! The `[review]` table: limits for `coda review`.

use serde::Deserialize;

/// Key of the table in `config.toml`.
pub const REVIEW_KEY: &str = "review";
/// Key of the finding cap inside `[review]`.
pub const MAX_FINDINGS_KEY: &str = "max_findings";
/// Findings kept per run when the file does not say.
pub const DEFAULT_MAX_FINDINGS: usize = 5;

/// The `[review]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ReviewConfig {
    /// Most findings one review run reports. Must not be zero.
    pub max_findings: usize,
    /// Files left out of a review.
    pub ignore: ReviewIgnore,
    /// Pull requests that are not reviewed.
    pub skip: ReviewSkip,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            max_findings: DEFAULT_MAX_FINDINGS,
            ignore: ReviewIgnore::default(),
            skip: ReviewSkip::default(),
        }
    }
}

/// The `[review.ignore]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ReviewIgnore {
    /// Globs of files to leave out. A glob with no `/` matches the file name at any depth.
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
