// SPDX-License-Identifier: Apache-2.0

//! What `coda review --post` sends to a pull request: inline comments for the findings that
//! sit on a line of the diff, and one summary comment for the rest. Nothing here talks to
//! GitHub; the caller does that and tells this module what is already there.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use crate::render::{coverage_notes_markdown, finding_markdown, overview_markdown};
use crate::schema::{DiffScope, Finding, Review};

/// The hidden line that marks the summary comment, so a rerun finds it and edits it.
pub const SUMMARY_MARKER: &str = "<!-- coda-review:summary -->";
const FINGERPRINT_OPEN: &str = "<!-- coda-review:fp=";
const MARKER_CLOSE: &str = " -->";
/// Hex characters of the digest kept in a fingerprint.
const FINGERPRINT_LEN: usize = 16;
const FIELD_SEPARATOR: char = '\0';

/// An inline comment on one line, or one range of lines, of the new side of the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineComment {
    /// Path of the file.
    pub path: String,
    /// First line of the range. Equal to `line` for a single line.
    pub start_line: u32,
    /// Last line of the range.
    pub line: u32,
    /// The comment text, ending with the hidden fingerprint.
    pub body: String,
    /// The finding's fingerprint.
    pub fingerprint: String,
    /// The finding's headline, for the preview.
    pub title: String,
}

/// What a post would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostPlan {
    /// Comments not on the pull request yet.
    pub inline: Vec<InlineComment>,
    /// Findings whose comment is already there and is skipped.
    pub already_posted: usize,
    /// Findings that are on no line of the diff and go in the summary.
    pub in_summary: usize,
    /// The text of the summary comment, starting with [`SUMMARY_MARKER`].
    pub summary: String,
}

/// A short stable id of a finding: its file, its category and its headline with case,
/// spacing and punctuation ignored. The same finding on a later run gets the same id even
/// when the model words the explanation differently.
#[must_use]
pub fn fingerprint(finding: &Finding) -> String {
    let title: String = finding
        .title
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    let text = format!(
        "{}{FIELD_SEPARATOR}{}{FIELD_SEPARATOR}{title}",
        finding.file,
        finding.category.as_str()
    );
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in &digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex.truncate(FINGERPRINT_LEN);
    hex
}

/// The fingerprint a comment body carries, when it has one.
#[must_use]
pub fn fingerprint_in(body: &str) -> Option<&str> {
    let body = body.trim_end().strip_suffix(MARKER_CLOSE)?;
    let start = body.rfind(FINGERPRINT_OPEN)? + FINGERPRINT_OPEN.len();
    Some(&body[start..])
}

/// Decides what to post. A finding whose lines are all inside the diff of one of `scopes`
/// becomes an inline comment, unless its fingerprint is in `posted`; every other finding
/// goes in the summary.
#[must_use]
pub fn plan(review: &Review, scopes: &[&DiffScope], posted: &BTreeSet<String>) -> PostPlan {
    let mut inline: Vec<InlineComment> = Vec::new();
    let mut outside: Vec<&Finding> = Vec::new();
    let mut already_posted = 0;
    for finding in &review.findings {
        let on_diff = scopes
            .iter()
            .any(|s| s.contains(&finding.file, finding.start_line, finding.end_line));
        if !on_diff {
            outside.push(finding);
            continue;
        }
        let print = fingerprint(finding);
        if posted.contains(&print) {
            already_posted += 1;
            continue;
        }
        if inline.iter().any(|c| c.fingerprint == print) {
            outside.push(finding);
            continue;
        }
        inline.push(InlineComment {
            path: finding.file.clone(),
            start_line: finding.start_line,
            line: finding.end_line,
            body: format!(
                "{}{FINGERPRINT_OPEN}{print}{MARKER_CLOSE}",
                finding_markdown(finding)
            ),
            fingerprint: print,
            title: finding.title.clone(),
        });
    }
    PostPlan {
        inline,
        already_posted,
        in_summary: outside.len(),
        summary: summary_text(review, &outside),
    }
}

fn summary_text(review: &Review, outside: &[&Finding]) -> String {
    let mut out = format!(
        "{SUMMARY_MARKER}\n## Review\n\n{}",
        overview_markdown(review)
    );
    out.push_str(&coverage_notes_markdown(review));
    if outside.is_empty() && !review.run.partial {
        out.push_str("Every finding is a comment on the diff.\n");
        return out;
    }
    let _ = write!(
        out,
        "## Findings not on a line of the diff ({})\n\n",
        outside.len()
    );
    for finding in outside {
        out.push_str(&finding_markdown(finding));
    }
    out
}

/// What the user is asked to confirm.
#[must_use]
pub fn preview(plan: &PostPlan, summary_exists: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Pending review with {} inline comment(s); {} already posted and skipped.",
        plan.inline.len(),
        plan.already_posted
    );
    for c in &plan.inline {
        let _ = writeln!(out, "  {}:{} {}", c.path, c.line, one_line(&c.title));
    }
    let action = if summary_exists { "update" } else { "create" };
    let _ = writeln!(
        out,
        "Summary comment: {action} ({} finding(s) outside the diff).",
        plan.in_summary
    );
    out
}

fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Category, Confidence, Coverage, Risk, RunDetails, Severity};

    fn finding(file: &str, line: u32, title: &str) -> Finding {
        Finding {
            id: "F1".to_owned(),
            file: file.to_owned(),
            start_line: line,
            end_line: line,
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

    fn review(findings: Vec<Finding>) -> Review {
        Review {
            summary: "Adds things.".to_owned(),
            effort: 2,
            risk: Risk::Medium,
            findings,
            coverage: Coverage::default(),
            run: RunDetails::default(),
        }
    }

    fn scope() -> DiffScope {
        let mut scope = DiffScope::new();
        scope.add("a.rs", 10, 20);
        scope
    }

    #[test]
    fn a_finding_on_a_diff_line_is_an_inline_comment_with_its_fingerprint() {
        // Arrange
        let f = finding("a.rs", 12, "Off by one");
        let review = review(vec![f.clone()]);

        // Act
        let plan = plan(&review, &[&scope()], &BTreeSet::new());

        // Assert
        assert_eq!(plan.inline.len(), 1);
        assert_eq!(plan.inline[0].line, 12);
        assert_eq!(
            fingerprint_in(&plan.inline[0].body),
            Some(fingerprint(&f).as_str())
        );
        assert_eq!(plan.in_summary, 0);
    }

    #[test]
    fn a_finding_outside_the_diff_goes_to_the_summary_only() {
        // Arrange
        let review = review(vec![
            finding("a.rs", 99, "Far away"),
            finding("b.rs", 1, "Other file"),
        ]);

        // Act
        let plan = plan(&review, &[&scope()], &BTreeSet::new());

        // Assert
        assert_eq!(plan.inline.len(), 0);
        assert_eq!(plan.in_summary, 2);
        assert!(plan.summary.starts_with(SUMMARY_MARKER));
        assert!(plan.summary.contains("Far away"));
        assert!(plan.summary.contains("Other file"));
    }

    #[test]
    fn a_finding_already_posted_is_skipped() {
        // Arrange
        let f = finding("a.rs", 12, "Off by one");
        let posted = BTreeSet::from([fingerprint(&f)]);

        // Act
        let plan = plan(&review(vec![f]), &[&scope()], &posted);

        // Assert
        assert_eq!(plan.inline.len(), 0);
        assert_eq!(plan.already_posted, 1);
        assert_eq!(plan.in_summary, 0);
    }

    #[test]
    fn a_second_finding_with_one_fingerprint_goes_to_the_summary_not_nowhere() {
        // Arrange
        let review = review(vec![
            finding("a.rs", 12, "Same"),
            finding("a.rs", 15, "same!"),
        ]);

        // Act
        let plan = plan(&review, &[&scope()], &BTreeSet::new());

        // Assert
        assert_eq!(plan.inline.len(), 1);
        assert_eq!(plan.already_posted, 0);
        assert_eq!(plan.in_summary, 1);
        assert!(plan.summary.contains("same!"));
    }

    #[test]
    fn a_marker_inside_a_patch_does_not_hide_the_real_fingerprint() {
        // Arrange
        let mut f = finding("a.rs", 12, "Off by one");
        f.suggested_patch = Some("<!-- coda-review:fp=fake -->".to_owned());
        let plan = plan(&review(vec![f.clone()]), &[&scope()], &BTreeSet::new());

        // Act and Assert
        assert_eq!(
            fingerprint_in(&plan.inline[0].body),
            Some(fingerprint(&f).as_str())
        );
    }

    #[test]
    fn a_partial_review_says_so_in_the_summary() {
        // Arrange
        let mut r = review(vec![]);
        r.run.partial = true;
        r.coverage.left_out = vec!["lost.rs".to_owned()];

        // Act
        let plan = plan(&r, &[&scope()], &BTreeSet::new());

        // Assert
        assert!(plan.summary.contains("Partial review"));
        assert!(plan.summary.contains("lost.rs"));
        assert!(!plan.summary.contains("Every finding is a comment"));
    }

    #[test]
    fn the_fingerprint_ignores_case_spacing_punctuation_and_the_explanation() {
        // Arrange
        let a = finding("a.rs", 12, "Off by one");
        let mut b = finding("a.rs", 40, "off  BY one!");
        b.explanation = "worded another way".to_owned();
        let other_file = finding("b.rs", 12, "Off by one");

        // Act and Assert
        assert_eq!(fingerprint(&a), fingerprint(&b));
        assert_ne!(fingerprint(&a), fingerprint(&other_file));
        assert_eq!(fingerprint(&a).len(), FINGERPRINT_LEN);
    }

    #[test]
    fn a_body_without_a_fingerprint_has_none() {
        // Act and Assert
        assert_eq!(fingerprint_in("just a comment"), None);
        assert_eq!(fingerprint_in("<!-- coda-review:fp=abc"), None);
        assert_eq!(fingerprint_in("x <!-- coda-review:fp=abc -->"), Some("abc"));
    }

    #[test]
    fn a_range_comment_keeps_both_ends() {
        // Arrange
        let mut f = finding("a.rs", 12, "Range");
        f.end_line = 14;

        // Act
        let plan = plan(&review(vec![f]), &[&scope()], &BTreeSet::new());

        // Assert
        assert_eq!((plan.inline[0].start_line, plan.inline[0].line), (12, 14));
    }

    #[test]
    fn the_preview_names_each_comment_and_the_summary_action() {
        // Arrange
        let review = review(vec![
            finding("a.rs", 12, "Off by one"),
            finding("a.rs", 99, "Far"),
        ]);
        let plan = plan(&review, &[&scope()], &BTreeSet::new());

        // Act
        let create = preview(&plan, false);
        let update = preview(&plan, true);

        // Assert
        assert!(create.contains("a.rs:12 Off by one"));
        assert!(create.contains("Summary comment: create (1 finding(s)"));
        assert!(update.contains("Summary comment: update"));
    }
}
