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

/// A branch ref passed to [`read_default_branch_rules`] starts with one of these, which keeps
/// git from looking the name up among tags first.
const BRANCH_REF_PREFIXES: [&str; 2] = ["refs/heads/", "refs/remotes/"];

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
    /// The default branch ref is not a fully qualified ref name or does not resolve.
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
/// when both sides keep it. The finding cap and the diff budget come from the global config alone.
#[must_use]
pub fn merge(global: &ReviewConfig, repo: Option<&ReviewConfig>) -> ReviewConfig {
    let Some(repo) = repo else {
        return global.clone();
    };
    let join = |a: &[String], b: &[String]| [a, b].concat();
    ReviewConfig {
        max_findings: global.max_findings,
        diff_budget_chars: global.diff_budget_chars,
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

fn is_valid_ref_name(repo_dir: &Path, name: &str) -> Result<bool, RulesError> {
    Ok(git(repo_dir, &["check-ref-format", name])?.0)
}

/// Reads the rules file from `default_ref`, the repository's default branch, as a fully
/// qualified ref such as `refs/remotes/origin/main`, so that a tag with the branch's name
/// cannot take its place. Only `refs/heads/` and `refs/remotes/` refs are accepted. The
/// working tree and the pull request branch are not consulted. Returns `None` when the default
/// branch has no rules file.
///
/// # Errors
/// Returns [`RulesError`] when the ref does not resolve, git fails or the file is bad.
pub fn read_default_branch_rules(
    repo_dir: &Path,
    default_ref: &str,
) -> Result<Option<ReviewConfig>, RulesError> {
    if !BRANCH_REF_PREFIXES
        .iter()
        .any(|prefix| default_ref.starts_with(prefix))
        || !is_valid_ref_name(repo_dir, default_ref)?
    {
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
        let rules = load(&ReviewConfig::default(), dir.path(), "refs/heads/main").unwrap();

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
        let found = read_default_branch_rules(dir.path(), "refs/heads/main").unwrap();

        // Assert
        assert_eq!(found, None);
    }

    #[test]
    fn an_unknown_or_option_like_ref_is_an_error() {
        // Arrange
        let dir = repo_with_head_branch_rules();

        // Act and Assert
        for bad in [
            "nope",
            "--output=x",
            "",
            "main",
            "refs/heads/nope",
            "refs/heads/pr~1",
            "refs/heads/ma in",
            "refs/tags/main",
            "refs/pull/5/head",
        ] {
            let err = read_default_branch_rules(dir.path(), bad).unwrap_err();
            assert!(matches!(err, RulesError::BadRef(_)), "{bad}");
        }
    }

    #[test]
    fn a_tag_named_like_the_branch_cannot_win() {
        // Arrange
        let dir = repo_with_head_branch_rules();
        let p = dir.path();
        run(p, &["-c", "tag.gpgsign=false", "tag", "main", "pr"]);

        // Act
        let by_name = read_default_branch_rules(p, "main");
        let by_ref = read_default_branch_rules(p, "refs/heads/main")
            .unwrap()
            .unwrap();

        // Assert
        assert!(matches!(by_name, Err(RulesError::BadRef(_))));
        let by_tag = read_default_branch_rules(p, "refs/tags/main");
        assert!(matches!(by_tag, Err(RulesError::BadRef(_))));
        assert_eq!(by_ref.ignore.globs, ["docs/**"]);
        assert!(!by_ref.skip.draft);
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

    fn full(tag: &str) -> ReviewConfig {
        let mut config = ReviewConfig::default();
        config.ignore.globs = vec![format!("{tag}/**")];
        config.skip.labels = vec![format!("{tag}-label")];
        config.skip.title_regex = vec![format!("{tag}-title")];
        config.skip.source_branch_regex = vec![format!("{tag}-source")];
        config.skip.target_branch_regex = vec![format!("{tag}-target")];
        config.skip.authors = vec![format!("{tag}-author")];
        config
    }

    #[test]
    fn merging_joins_every_list_and_keeps_the_global_numbers() {
        // Arrange
        let mut global = full("g");
        global.max_findings = 9;
        global.diff_budget_chars = 777;
        let mut repo = full("r");
        repo.diff_budget_chars = 5;

        // Act
        let merged = merge(&global, Some(&repo));

        // Assert
        assert_eq!(merged.diff_budget_chars, 777);
        assert_eq!(merged.ignore.globs, ["g/**", "r/**"]);
        assert_eq!(merged.skip.labels, ["g-label", "r-label"]);
        assert_eq!(merged.skip.title_regex, ["g-title", "r-title"]);
        assert_eq!(merged.skip.source_branch_regex, ["g-source", "r-source"]);
        assert_eq!(merged.skip.target_branch_regex, ["g-target", "r-target"]);
        assert_eq!(merged.skip.authors, ["g-author", "r-author"]);
    }

    #[test]
    fn merging_turns_a_switch_on_when_either_side_has_it() {
        // Arrange
        let off = ReviewConfig::default();
        let mut bots = ReviewConfig::default();
        bots.skip.bots = true;
        let mut draft = ReviewConfig::default();
        draft.skip.draft = true;

        for (global, repo) in [(&bots, &off), (&off, &bots)] {
            // Act
            let merged = merge(global, Some(repo));

            // Assert
            assert!(merged.skip.bots);
            assert!(!merged.skip.draft);
        }
        for (global, repo) in [(&draft, &off), (&off, &draft)] {
            // Act
            let merged = merge(global, Some(repo));

            // Assert
            assert!(merged.skip.draft);
            assert!(!merged.skip.bots);
        }
        assert!(!merge(&off, Some(&off)).skip.bots);
        let mut no_builtin = ReviewConfig::default();
        no_builtin.ignore.builtin = false;
        assert!(!merge(&off, Some(&no_builtin)).ignore.builtin);
        assert!(!merge(&no_builtin, Some(&off)).ignore.builtin);
    }

    #[test]
    fn without_repo_rules_the_global_config_comes_back_unchanged() {
        // Arrange
        let mut global = full("g");
        global.skip.bots = true;
        global.ignore.builtin = false;

        // Act and Assert
        assert_eq!(merge(&global, None), global);
    }

    #[test]
    fn compiled_rules_keep_the_builtin_list_only_when_asked() {
        // Arrange
        let mut off = ReviewConfig::default();
        off.ignore.builtin = false;

        // Act and Assert
        assert!(
            Rules::compile(&ReviewConfig::default())
                .unwrap()
                .ignore
                .is_ignored("Cargo.lock")
        );
        assert!(
            !Rules::compile(&off)
                .unwrap()
                .ignore
                .is_ignored("Cargo.lock")
        );
    }
}
