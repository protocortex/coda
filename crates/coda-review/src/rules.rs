// SPDX-License-Identifier: Apache-2.0

//! Loading review rules. Rules come from the global config and from the repository's
//! default branch. A rules file on the pull request's own branch is never read, because
//! the author of the pull request could use it to switch the review off.

use std::path::Path;
use std::process::Command;

use coda_config::review::{ReviewConfig, ReviewIgnore, ReviewSkip};
use serde::Deserialize;

use crate::ignore::IgnoreSet;
use crate::skip::SkipRules;

/// Path of the repository rules file, read from the default branch only.
pub const REPO_RULES_PATH: &str = ".coda/review.toml";

/// Why the rules could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    /// A glob in `[review.ignore]` does not parse.
    #[error("review.ignore.globs: bad glob {pattern:?}: {reason}")]
    BadGlob {
        /// The glob as written.
        pattern: String,
        /// What is wrong with it.
        reason: String,
    },
    /// A regular expression in `[review.skip]` does not parse.
    #[error("review.skip.{key}: bad regex {pattern:?}: {reason}")]
    BadRegex {
        /// The config key.
        key: &'static str,
        /// The expression as written.
        pattern: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The rules file on the default branch is not valid TOML for the schema.
    #[error("{REPO_RULES_PATH} on the default branch: {0}")]
    BadRepoFile(String),
    /// The default branch ref is missing or looks like an option.
    #[error("default branch ref {0:?} cannot be resolved")]
    BadRef(String),
    /// Running git failed.
    #[error("git failed: {0}")]
    Git(String),
}

/// Compiled rules.
#[derive(Debug)]
pub struct Rules {
    /// Files to leave out.
    pub ignore: IgnoreSet,
    /// Pull requests to skip.
    pub skip: SkipRules,
}

impl Rules {
    /// Compiles the ignore and skip rules of `config`.
    ///
    /// # Errors
    /// Returns [`RulesError`] for a glob or regex that does not parse.
    pub fn compile(config: &ReviewConfig) -> Result<Self, RulesError> {
        Ok(Self {
            ignore: IgnoreSet::new(&config.ignore.globs, config.ignore.builtin)?,
            skip: SkipRules::compile(&config.skip)?,
        })
    }
}

#[derive(Deserialize, Default)]
struct RepoFile {
    #[serde(default)]
    review: RepoReview,
}

#[derive(Deserialize, Default)]
struct RepoReview {
    #[serde(default)]
    ignore: ReviewIgnore,
    #[serde(default)]
    skip: ReviewSkip,
}

/// Parses the text of a repository rules file. Only `[review.ignore]` and
/// `[review.skip]` are read from it.
///
/// # Errors
/// Returns [`RulesError::BadRepoFile`] when the text does not fit the schema.
pub fn parse_repo_rules(text: &str) -> Result<ReviewConfig, RulesError> {
    let file: RepoFile =
        toml::from_str(text).map_err(|e| RulesError::BadRepoFile(e.to_string()))?;
    Ok(ReviewConfig {
        ignore: file.review.ignore,
        skip: file.review.skip,
        ..ReviewConfig::default()
    })
}

/// Combines the global config with the repository rules. Lists add up, a switch that
/// skips is on when either side turns it on, and the built-in ignore list stays on only
/// when both sides keep it. The finding cap comes from the global config alone.
#[must_use]
pub fn merge(global: &ReviewConfig, repo: Option<&ReviewConfig>) -> ReviewConfig {
    let Some(repo) = repo else {
        return global.clone();
    };
    let join = |a: &[String], b: &[String]| [a, b].concat();
    ReviewConfig {
        max_findings: global.max_findings,
        ignore: ReviewIgnore {
            globs: join(&global.ignore.globs, &repo.ignore.globs),
            builtin: global.ignore.builtin && repo.ignore.builtin,
        },
        skip: ReviewSkip {
            draft: global.skip.draft || repo.skip.draft,
            labels: join(&global.skip.labels, &repo.skip.labels),
            title_regex: join(&global.skip.title_regex, &repo.skip.title_regex),
            source_branch_regex: join(
                &global.skip.source_branch_regex,
                &repo.skip.source_branch_regex,
            ),
            target_branch_regex: join(
                &global.skip.target_branch_regex,
                &repo.skip.target_branch_regex,
            ),
            authors: join(&global.skip.authors, &repo.skip.authors),
            bots: global.skip.bots || repo.skip.bots,
        },
    }
}

fn git(repo_dir: &Path, args: &[&str]) -> Result<(bool, Vec<u8>), RulesError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .map_err(|e| RulesError::Git(e.to_string()))?;
    Ok((out.status.success(), out.stdout))
}

/// Reads the rules file from `default_ref`, the repository's default branch. The working
/// tree and the pull request branch are not consulted. Returns `None` when the default
/// branch has no rules file.
///
/// # Errors
/// Returns [`RulesError`] when the ref does not resolve, git fails or the file is bad.
pub fn read_default_branch_rules(
    repo_dir: &Path,
    default_ref: &str,
) -> Result<Option<ReviewConfig>, RulesError> {
    if default_ref.is_empty() || default_ref.starts_with('-') {
        return Err(RulesError::BadRef(default_ref.to_owned()));
    }
    let commit = format!("{default_ref}^{{commit}}");
    if !git(repo_dir, &["rev-parse", "--verify", "--quiet", &commit])?.0 {
        return Err(RulesError::BadRef(default_ref.to_owned()));
    }
    let object = format!("{default_ref}:{REPO_RULES_PATH}");
    if !git(repo_dir, &["cat-file", "-e", &object])?.0 {
        return Ok(None);
    }
    let (ok, bytes) = git(repo_dir, &["show", &object])?;
    if !ok {
        return Err(RulesError::Git(format!("could not read {object}")));
    }
    let text = String::from_utf8(bytes).map_err(|e| RulesError::BadRepoFile(e.to_string()))?;
    parse_repo_rules(&text).map(Some)
}

/// Loads the rules for a review: the global config plus the default branch's rules file.
///
/// # Errors
/// Returns [`RulesError`] from reading or compiling the rules.
pub fn load(
    global: &ReviewConfig,
    repo_dir: &Path,
    default_ref: &str,
) -> Result<Rules, RulesError> {
    let repo = read_default_branch_rules(repo_dir, default_ref)?;
    Rules::compile(&merge(global, repo.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn repo_with_head_branch_rules() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        run(p, &["init", "-q", "-b", "main"]);
        write(p, "a.txt", "a");
        write(
            p,
            REPO_RULES_PATH,
            "[review.ignore]\nglobs = [\"docs/**\"]\n",
        );
        run(p, &["add", "."]);
        run(p, &["commit", "-q", "-m", "base"]);
        run(p, &["checkout", "-q", "-b", "pr"]);
        write(
            p,
            REPO_RULES_PATH,
            "[review.skip]\ndraft = true\nbots = true\n[review.ignore]\nglobs = [\"**\"]\n",
        );
        run(p, &["commit", "-q", "-am", "pr rules"]);
        dir
    }

    #[test]
    fn a_rules_file_on_the_pr_branch_is_ignored() {
        // Arrange
        let dir = repo_with_head_branch_rules();

        // Act
        let rules = load(&ReviewConfig::default(), dir.path(), "main").unwrap();

        // Assert
        assert!(!rules.ignore.is_ignored("src/lib.rs"));
        assert!(rules.ignore.is_ignored("docs/a.md"));
        assert_eq!(
            rules.skip,
            SkipRules::compile(&ReviewSkip::default()).unwrap()
        );
    }

    #[test]
    fn a_default_branch_without_a_rules_file_gives_none() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q", "-b", "main"]);
        write(dir.path(), "a.txt", "a");
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "base"]);

        // Act
        let found = read_default_branch_rules(dir.path(), "main").unwrap();

        // Assert
        assert_eq!(found, None);
    }

    #[test]
    fn an_unknown_or_option_like_ref_is_an_error() {
        // Arrange
        let dir = repo_with_head_branch_rules();

        // Act and Assert
        for bad in ["nope", "--output=x", ""] {
            let err = read_default_branch_rules(dir.path(), bad).unwrap_err();
            assert!(matches!(err, RulesError::BadRef(_)), "{bad}");
        }
    }

    #[test]
    fn merging_adds_lists_and_ors_switches_but_keeps_the_global_cap() {
        // Arrange
        let mut global = ReviewConfig {
            max_findings: 9,
            ..ReviewConfig::default()
        };
        global.ignore.globs = vec!["a/**".to_owned()];
        global.skip.labels = vec!["wip".to_owned()];
        let repo = parse_repo_rules(
            "[review]\nmax_findings = 1\n[review.ignore]\nglobs = [\"b/**\"]\nbuiltin = false\n\
             [review.skip]\ndraft = true\nlabels = [\"hold\"]\n",
        )
        .unwrap();

        // Act
        let merged = merge(&global, Some(&repo));

        // Assert
        assert_eq!(merged.max_findings, 9);
        assert_eq!(merged.ignore.globs, ["a/**", "b/**"]);
        assert!(!merged.ignore.builtin);
        assert!(merged.skip.draft);
        assert_eq!(merged.skip.labels, ["wip", "hold"]);
    }

    #[test]
    fn a_bad_repo_file_names_the_file() {
        // Arrange and Act
        let err = parse_repo_rules("[review.skip]\ndraft = \"yes\"\n").unwrap_err();

        // Assert
        assert!(err.to_string().contains(REPO_RULES_PATH), "{err}");
    }
}
