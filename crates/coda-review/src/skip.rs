// SPDX-License-Identifier: Apache-2.0

//! Pull-request level skip rules.

use coda_config::review::ReviewSkip;
use regex::Regex;

use crate::rules::RulesError;

/// Suffix GitHub puts on the login of an app account.
const BOT_LOGIN_SUFFIX: &str = "[bot]";

/// The facts about a pull request that the skip rules look at. All text is untrusted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrInfo {
    /// Whether the pull request is a draft.
    pub draft: bool,
    /// Label names.
    pub labels: Vec<String>,
    /// Title.
    pub title: String,
    /// Branch the change comes from.
    pub source_branch: String,
    /// Branch the change goes to.
    pub target_branch: String,
    /// Login of the author.
    pub author: String,
    /// Whether the host says the author is a bot account.
    pub author_is_bot: bool,
}

impl PrInfo {
    fn is_bot(&self) -> bool {
        self.author_is_bot || self.author.ends_with(BOT_LOGIN_SUFFIX)
    }
}

/// Why a pull request is skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The pull request is a draft.
    Draft,
    /// It carries a listed label.
    Label(String),
    /// The title matches a listed expression.
    Title,
    /// The source branch matches a listed expression.
    SourceBranch,
    /// The target branch matches a listed expression.
    TargetBranch,
    /// The author is on the list.
    Author(String),
    /// The author is a bot.
    Bot,
}

impl SkipReason {
    /// A one-line message. Control characters in a label or login are dropped.
    #[must_use]
    pub fn one_line(&self) -> String {
        let why = match self {
            Self::Draft => "the pull request is a draft".to_owned(),
            Self::Label(label) => format!("it has the label {}", sanitize(label)),
            Self::Title => "the title matches a review.skip.title_regex rule".to_owned(),
            Self::SourceBranch => {
                "the source branch matches a review.skip.source_branch_regex rule".to_owned()
            }
            Self::TargetBranch => {
                "the target branch matches a review.skip.target_branch_regex rule".to_owned()
            }
            Self::Author(login) => {
                format!("the author {} is in review.skip.authors", sanitize(login))
            }
            Self::Bot => "the author is a bot".to_owned(),
        };
        format!("review skipped: {why} (use --force to review anyway)")
    }
}

fn sanitize(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// What to do with a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Review it.
    Review,
    /// Review it although a rule matched, because of `--force`.
    Forced(SkipReason),
    /// Do not review it; the command exits 0 and prints the reason.
    Skip(SkipReason),
}

/// Compiled skip rules.
#[derive(Debug, Clone)]
pub struct SkipRules {
    draft: bool,
    labels: Vec<String>,
    title: Vec<Regex>,
    source_branch: Vec<Regex>,
    target_branch: Vec<Regex>,
    authors: Vec<String>,
    bots: bool,
}

impl PartialEq for SkipRules {
    fn eq(&self, other: &Self) -> bool {
        let patterns = |v: &[Regex]| v.iter().map(|r| r.as_str().to_owned()).collect::<Vec<_>>();
        self.draft == other.draft
            && self.labels == other.labels
            && patterns(&self.title) == patterns(&other.title)
            && patterns(&self.source_branch) == patterns(&other.source_branch)
            && patterns(&self.target_branch) == patterns(&other.target_branch)
            && self.authors == other.authors
            && self.bots == other.bots
    }
}

fn compile_all(key: &'static str, patterns: &[String]) -> Result<Vec<Regex>, RulesError> {
    patterns
        .iter()
        .map(|pattern| {
            Regex::new(pattern).map_err(|e| RulesError::BadRegex {
                key,
                pattern: pattern.clone(),
                reason: e.to_string(),
            })
        })
        .collect()
}

fn lowercase(items: &[String]) -> Vec<String> {
    items.iter().map(|s| s.to_lowercase()).collect()
}

impl SkipRules {
    /// Compiles the rules.
    ///
    /// # Errors
    /// Returns [`RulesError::BadRegex`] for an expression that does not parse.
    pub fn compile(config: &ReviewSkip) -> Result<Self, RulesError> {
        Ok(Self {
            draft: config.draft,
            labels: lowercase(&config.labels),
            title: compile_all("title_regex", &config.title_regex)?,
            source_branch: compile_all("source_branch_regex", &config.source_branch_regex)?,
            target_branch: compile_all("target_branch_regex", &config.target_branch_regex)?,
            authors: lowercase(&config.authors),
            bots: config.bots,
        })
    }

    /// The first rule that matches `pr`, if any.
    #[must_use]
    pub fn first_match(&self, pr: &PrInfo) -> Option<SkipReason> {
        if self.draft && pr.draft {
            return Some(SkipReason::Draft);
        }
        if let Some(label) = pr
            .labels
            .iter()
            .find(|l| self.labels.contains(&l.to_lowercase()))
        {
            return Some(SkipReason::Label(label.clone()));
        }
        if self.title.iter().any(|r| r.is_match(&pr.title)) {
            return Some(SkipReason::Title);
        }
        if self
            .source_branch
            .iter()
            .any(|r| r.is_match(&pr.source_branch))
        {
            return Some(SkipReason::SourceBranch);
        }
        if self
            .target_branch
            .iter()
            .any(|r| r.is_match(&pr.target_branch))
        {
            return Some(SkipReason::TargetBranch);
        }
        if self.authors.contains(&pr.author.to_lowercase()) {
            return Some(SkipReason::Author(pr.author.clone()));
        }
        if self.bots && pr.is_bot() {
            return Some(SkipReason::Bot);
        }
        None
    }

    /// Decides what to do with `pr`. `force` overrides a skip.
    #[must_use]
    pub fn decide(&self, pr: &PrInfo, force: bool) -> Decision {
        match (self.first_match(pr), force) {
            (None, _) => Decision::Review,
            (Some(reason), true) => Decision::Forced(reason),
            (Some(reason), false) => Decision::Skip(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr() -> PrInfo {
        PrInfo {
            draft: false,
            labels: vec!["Bug".to_owned()],
            title: "fix: a thing".to_owned(),
            source_branch: "feature/x".to_owned(),
            target_branch: "main".to_owned(),
            author: "alice".to_owned(),
            author_is_bot: false,
        }
    }

    fn rules(edit: impl FnOnce(&mut ReviewSkip)) -> SkipRules {
        let mut config = ReviewSkip::default();
        edit(&mut config);
        SkipRules::compile(&config).unwrap()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn no_rules_means_review() {
        // Arrange
        let rules = rules(|_| {});

        // Act and Assert
        assert_eq!(rules.decide(&pr(), false), Decision::Review);
    }

    #[test]
    fn each_rule_skips_with_its_reason() {
        // Arrange
        let cases: Vec<(SkipRules, PrInfo, SkipReason)> = vec![
            (
                rules(|c| c.draft = true),
                PrInfo {
                    draft: true,
                    ..pr()
                },
                SkipReason::Draft,
            ),
            (
                rules(|c| c.labels = strings(&["skip-review", "BUG"])),
                pr(),
                SkipReason::Label("Bug".to_owned()),
            ),
            (
                rules(|c| c.title_regex = strings(&["^fix:"])),
                pr(),
                SkipReason::Title,
            ),
            (
                rules(|c| c.source_branch_regex = strings(&["^feature/"])),
                pr(),
                SkipReason::SourceBranch,
            ),
            (
                rules(|c| c.target_branch_regex = strings(&["^(main|release/.*)$"])),
                pr(),
                SkipReason::TargetBranch,
            ),
            (
                rules(|c| c.authors = strings(&["ALICE"])),
                pr(),
                SkipReason::Author("alice".to_owned()),
            ),
            (
                rules(|c| c.bots = true),
                PrInfo {
                    author_is_bot: true,
                    ..pr()
                },
                SkipReason::Bot,
            ),
            (
                rules(|c| c.bots = true),
                PrInfo {
                    author: "dependabot[bot]".to_owned(),
                    ..pr()
                },
                SkipReason::Bot,
            ),
        ];

        for (rules, pr, reason) in cases {
            // Act
            let decision = rules.decide(&pr, false);

            // Assert
            assert_eq!(decision, Decision::Skip(reason));
        }
    }

    #[test]
    fn rules_that_do_not_match_review() {
        // Arrange
        let rules = rules(|c| {
            c.draft = true;
            c.labels = strings(&["wip"]);
            c.title_regex = strings(&["^chore"]);
            c.source_branch_regex = strings(&["^dependabot/"]);
            c.authors = strings(&["bob"]);
            c.bots = true;
        });

        // Act and Assert
        assert_eq!(rules.decide(&pr(), false), Decision::Review);
    }

    #[test]
    fn force_overrides_a_skip() {
        // Arrange
        let rules = rules(|c| c.title_regex = strings(&["fix"]));

        // Act
        let decision = rules.decide(&pr(), true);

        // Assert
        assert_eq!(decision, Decision::Forced(SkipReason::Title));
    }

    #[test]
    fn the_skip_message_is_one_line_even_with_hostile_text() {
        // Arrange
        let reason = SkipReason::Label("a\nb\u{1b}[31m".to_owned());

        // Act
        let line = reason.one_line();

        // Assert
        assert_eq!(line.lines().count(), 1);
        assert!(!line.contains('\u{1b}'));
        assert!(line.starts_with("review skipped: "));
    }

    #[test]
    fn a_bad_regex_names_its_key() {
        // Arrange
        let config = ReviewSkip {
            source_branch_regex: strings(&["("]),
            ..ReviewSkip::default()
        };

        // Act
        let err = SkipRules::compile(&config).unwrap_err();

        // Assert
        assert!(err.to_string().contains("source_branch_regex"), "{err}");
    }

    #[test]
    fn a_flag_that_is_off_never_skips() {
        // Arrange
        let off = rules(|c| {
            c.draft = false;
            c.bots = false;
        });
        let draft = PrInfo {
            draft: true,
            ..pr()
        };
        let bot = PrInfo {
            author_is_bot: true,
            author: "x[bot]".to_owned(),
            ..pr()
        };

        // Act and Assert
        assert_eq!(off.decide(&draft, false), Decision::Review);
        assert_eq!(off.decide(&bot, false), Decision::Review);
    }

    #[test]
    fn authors_match_without_regard_to_case_and_keep_the_pr_spelling() {
        // Arrange
        let rules = rules(|c| c.authors = strings(&["alice"]));
        let upper = PrInfo {
            author: "ALICE".to_owned(),
            ..pr()
        };
        let mixed = PrInfo {
            author: "Alice".to_owned(),
            ..pr()
        };

        // Act and Assert
        assert_eq!(
            rules.first_match(&upper),
            Some(SkipReason::Author("ALICE".to_owned()))
        );
        assert_eq!(
            rules.first_match(&mixed),
            Some(SkipReason::Author("Alice".to_owned()))
        );
    }

    #[test]
    fn the_first_matching_rule_in_order_wins() {
        // Arrange
        let mut config = ReviewSkip {
            draft: true,
            labels: strings(&["bug"]),
            title_regex: strings(&["fix"]),
            source_branch_regex: strings(&["feature"]),
            target_branch_regex: strings(&["main"]),
            authors: strings(&["alice"]),
            bots: true,
        };
        let mut pr = PrInfo {
            draft: true,
            author_is_bot: true,
            ..pr()
        };
        let expected = [
            SkipReason::Draft,
            SkipReason::Label("Bug".to_owned()),
            SkipReason::Title,
            SkipReason::SourceBranch,
            SkipReason::TargetBranch,
            SkipReason::Author("alice".to_owned()),
            SkipReason::Bot,
        ];

        for (n, reason) in expected.iter().enumerate() {
            // Act
            let got = SkipRules::compile(&config).unwrap().first_match(&pr);

            // Assert
            assert_eq!(got.as_ref(), Some(reason), "step {n}");
            match n {
                0 => pr.draft = false,
                1 => config.labels.clear(),
                2 => config.title_regex.clear(),
                3 => config.source_branch_regex.clear(),
                4 => config.target_branch_regex.clear(),
                _ => config.authors.clear(),
            }
        }
    }

    #[test]
    fn every_skip_message_is_one_exact_line() {
        // Arrange
        let hint = " (use --force to review anyway)";
        let cases = [
            (SkipReason::Draft, "the pull request is a draft"),
            (SkipReason::Label("wip".to_owned()), "it has the label wip"),
            (
                SkipReason::Title,
                "the title matches a review.skip.title_regex rule",
            ),
            (
                SkipReason::SourceBranch,
                "the source branch matches a review.skip.source_branch_regex rule",
            ),
            (
                SkipReason::TargetBranch,
                "the target branch matches a review.skip.target_branch_regex rule",
            ),
            (
                SkipReason::Author("bo\nb\u{1b}".to_owned()),
                "the author bob is in review.skip.authors",
            ),
            (SkipReason::Bot, "the author is a bot"),
        ];

        for (reason, why) in cases {
            // Act
            let line = reason.one_line();

            // Assert
            assert_eq!(line, format!("review skipped: {why}{hint}"));
        }
    }

    #[test]
    fn every_regex_key_is_named_in_its_error() {
        // Arrange
        let bad = strings(&["("]);
        let cases = [
            (
                ReviewSkip {
                    title_regex: bad.clone(),
                    ..ReviewSkip::default()
                },
                "title_regex",
            ),
            (
                ReviewSkip {
                    source_branch_regex: bad.clone(),
                    ..ReviewSkip::default()
                },
                "source_branch_regex",
            ),
            (
                ReviewSkip {
                    target_branch_regex: bad,
                    ..ReviewSkip::default()
                },
                "target_branch_regex",
            ),
        ];

        for (config, key) in cases {
            // Act
            let err = SkipRules::compile(&config).unwrap_err();

            // Assert
            assert!(
                matches!(err, RulesError::BadRegex { key: k, .. } if k == key),
                "{key}: {err}"
            );
        }
    }

    #[test]
    fn rules_compare_equal_only_when_every_part_is_equal() {
        // Arrange
        let base = ReviewSkip::default();
        let edits: [fn(&mut ReviewSkip); 7] = [
            |c| c.draft = true,
            |c| c.labels = vec!["x".to_owned()],
            |c| c.title_regex = vec!["x".to_owned()],
            |c| c.source_branch_regex = vec!["x".to_owned()],
            |c| c.target_branch_regex = vec!["x".to_owned()],
            |c| c.authors = vec!["x".to_owned()],
            |c| c.bots = true,
        ];
        let plain = SkipRules::compile(&base).unwrap();

        // Act and Assert
        assert_eq!(plain, SkipRules::compile(&base).unwrap());
        for (n, edit) in edits.into_iter().enumerate() {
            let mut changed = base.clone();
            edit(&mut changed);
            assert_ne!(plain, SkipRules::compile(&changed).unwrap(), "edit {n}");
        }
    }
}
