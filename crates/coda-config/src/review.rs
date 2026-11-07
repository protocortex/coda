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

/// Path of the routing rules inside `[review]`, as `routing.rules`.
pub const ROUTING_RULES_PATH: &str = "routing.rules";
/// Key of the model inside a routing rule.
pub const ROUTING_MODEL_KEY: &str = "model";

/// Lines of the repository's guidance files that go in the reviewer's prompt when the file
/// does not say.
pub const DEFAULT_GUIDANCE_LINES: usize = 500;

/// Commits an incremental review needs when the file does not say.
pub const DEFAULT_INCREMENTAL_MIN_COMMITS: usize = 1;
/// Minutes an incremental review waits after the last run when the file does not say.
pub const DEFAULT_INCREMENTAL_MIN_MINUTES: u64 = 0;

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
    /// An incremental run (`coda review --incremental`) does nothing unless at least this
    /// many commits came after the last reviewed one.
    pub incremental_min_commits: usize,
    /// An incremental run does nothing until this many minutes passed since the last run.
    pub incremental_min_minutes: u64,
    /// Instructions added to every review's prompt, after the project rules.
    pub extra_instructions: String,
    /// Repositories (`org/repo`) whose issues a pull request may link to and have read
    /// into the review. Issues of the pull request's own repository are always read.
    pub linked_repos: Vec<String>,
    /// The review before a push, started by the hook `coda hooks install review` writes.
    pub hook: ReviewHook,
    /// Files left out of a review.
    pub ignore: ReviewIgnore,
    /// Pull requests that are not reviewed.
    pub skip: ReviewSkip,
    /// Which model reviews a part, by its size, and what happens when a backend runs out.
    pub routing: ReviewRouting,
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
            incremental_min_commits: DEFAULT_INCREMENTAL_MIN_COMMITS,
            incremental_min_minutes: DEFAULT_INCREMENTAL_MIN_MINUTES,
            extra_instructions: String::new(),
            linked_repos: Vec::new(),
            hook: ReviewHook::default(),
            ignore: ReviewIgnore::default(),
            skip: ReviewSkip::default(),
            routing: ReviewRouting::default(),
        }
    }
}

/// The `[review.hook]` section. Both switches are off until set, and only the global config
/// sets them: a repository's own rules file cannot turn a review on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ReviewHook {
    /// Whether the pre-push hook reviews anything. Off, the hook does nothing.
    pub enabled: bool,
    /// Whether a finding of severity high or critical stops the push.
    pub block_on_high: bool,
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

/// The `[review.routing]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewRouting {
    /// Rules tried in order for each part. The first one that fits sets the model. When none
    /// fits, the backend runs on its own default model.
    pub rules: Vec<RoutingRule>,
    /// Whether a part that hits a usage limit, or loses its backend, is tried once on the
    /// other backend that is enabled.
    pub fallback: bool,
}

impl Default for ReviewRouting {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            fallback: true,
        }
    }
}

/// One rule of `[review.routing]`. A rule fits a part when every limit it sets holds; a rule
/// with no limit fits every part.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingRule {
    /// Most hunks the part may have.
    #[serde(default)]
    pub max_hunks: Option<usize>,
    /// Most files the part may have.
    #[serde(default)]
    pub max_files: Option<usize>,
    /// Most characters of packed diff the part may have.
    #[serde(default)]
    pub max_chars: Option<usize>,
    /// The model the backend is set to. Must not be empty.
    pub model: String,
}

/// How big one part of a review is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartSize {
    /// Hunks in the part.
    pub hunks: usize,
    /// Files the part sends to the model, whole or clipped.
    pub files: usize,
    /// Characters of packed diff.
    pub chars: usize,
}

impl RoutingRule {
    fn fits(&self, size: PartSize) -> bool {
        self.max_hunks.is_none_or(|max| size.hunks <= max)
            && self.max_files.is_none_or(|max| size.files <= max)
            && self.max_chars.is_none_or(|max| size.chars <= max)
    }
}

impl ReviewRouting {
    /// The model for a part of `size`: that of the first rule that fits, or `None`.
    #[must_use]
    pub fn model_for(&self, size: PartSize) -> Option<&str> {
        self.rules
            .iter()
            .find(|rule| rule.fits(size))
            .map(|rule| rule.model.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: PartSize = PartSize {
        hunks: 2,
        files: 1,
        chars: 500,
    };
    const BIG: PartSize = PartSize {
        hunks: 40,
        files: 12,
        chars: 90_000,
    };

    fn rule(max_hunks: Option<usize>, model: &str) -> RoutingRule {
        RoutingRule {
            max_hunks,
            max_files: None,
            max_chars: None,
            model: model.to_owned(),
        }
    }

    fn routing(rules: Vec<RoutingRule>) -> ReviewRouting {
        ReviewRouting {
            rules,
            fallback: true,
        }
    }

    #[test]
    fn the_first_rule_that_fits_wins() {
        // Arrange
        let routing = routing(vec![
            rule(Some(5), "cheap"),
            rule(Some(50), "middle"),
            rule(None, "strong"),
        ]);

        // Act and Assert
        assert_eq!(routing.model_for(SMALL), Some("cheap"));
        assert_eq!(routing.model_for(BIG), Some("middle"));
    }

    #[test]
    fn an_earlier_catch_all_hides_the_later_rules() {
        // Arrange
        let routing = routing(vec![rule(None, "first"), rule(Some(5), "second")]);

        // Act and Assert
        assert_eq!(routing.model_for(SMALL), Some("first"));
    }

    #[test]
    fn no_rule_that_fits_leaves_the_backend_default() {
        // Arrange
        let routing = routing(vec![rule(Some(1), "cheap")]);

        // Act and Assert
        assert_eq!(routing.model_for(BIG), None);
        assert_eq!(ReviewRouting::default().model_for(SMALL), None);
    }

    #[test]
    fn a_rule_fits_only_when_every_limit_it_sets_holds() {
        // Arrange
        let both = RoutingRule {
            max_hunks: Some(10),
            max_files: Some(3),
            max_chars: Some(1000),
            model: "cheap".to_owned(),
        };
        let too_many_files = PartSize { files: 4, ..SMALL };
        let too_long = PartSize {
            chars: 1001,
            ..SMALL
        };
        let at_the_edge = PartSize {
            hunks: 10,
            files: 3,
            chars: 1000,
        };

        // Act and Assert
        assert!(both.fits(SMALL));
        assert!(!both.fits(too_many_files));
        assert!(!both.fits(too_long));
        assert!(both.fits(at_the_edge));
    }
}
