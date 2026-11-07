// SPDX-License-Identifier: Apache-2.0

//! The review output types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Lowest effort estimate.
pub const MIN_EFFORT: u8 = 1;
/// Highest effort estimate.
pub const MAX_EFFORT: u8 = 5;

/// How serious a finding is, most serious first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Breaks behavior or security; fix before merge.
    Critical,
    /// Likely bug or real weakness.
    High,
    /// Worth fixing.
    Medium,
    /// Minor.
    Low,
    /// A remark with no required action.
    Info,
}

impl Severity {
    /// The name used in JSON and markdown.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Info => "info",
        }
    }
}

impl Severity {
    /// Whether a finding this serious is one that can stop a push: high or critical.
    #[must_use]
    pub const fn blocks(self) -> bool {
        matches!(self, Self::Critical | Self::High)
    }
}

/// What kind of problem a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    /// Wrong behavior.
    Bug,
    /// Security weakness.
    Security,
    /// Slow or wasteful code.
    Performance,
    /// Hard to read or change.
    Maintainability,
    /// Missing or weak tests.
    Tests,
    /// Missing or wrong documentation.
    Docs,
    /// Anything else.
    Other,
}

impl Category {
    /// The name used in JSON and markdown.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bug => "bug",
            Self::Security => "security",
            Self::Performance => "performance",
            Self::Maintainability => "maintainability",
            Self::Tests => "tests",
            Self::Docs => "docs",
            Self::Other => "other",
        }
    }
}

/// How sure the model is about a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// A guess.
    Low,
    /// Probably right.
    Medium,
    /// Very likely right.
    High,
}

impl Confidence {
    /// The name used in JSON and markdown.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Overall risk of merging the change, lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    /// Safe to merge.
    Low,
    /// Needs a careful look.
    Medium,
    /// Likely to cause trouble.
    High,
}

impl Risk {
    /// The name used in JSON and markdown.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// How well the change meets one requirement of a linked issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// The change does not meet it.
    Not,
    /// The change meets part of it.
    Partially,
    /// The change meets all of it.
    Fully,
}

impl Verdict {
    /// The name used in JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Not => "not",
            Self::Partially => "partially",
            Self::Fully => "fully",
        }
    }

    /// The words the markdown uses.
    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::Not => "not compliant",
            Self::Partially => "partially compliant",
            Self::Fully => "fully compliant",
        }
    }
}

/// The verdict on one requirement of one linked issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComplianceItem {
    /// The issue, as the prompt names it: `#12` or `org/repo#12`.
    pub issue: String,
    /// The requirement, quoted from the issue.
    pub requirement: String,
    /// How well the change meets it.
    pub verdict: Verdict,
}

/// A finding as the model writes it. The id is added by coda, not the model.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawFinding {
    /// Path of the file, as in the diff.
    pub file: String,
    /// First line of the range, on the new side of the diff.
    pub start_line: u32,
    /// Last line of the range, inclusive.
    pub end_line: u32,
    /// How serious it is.
    pub severity: Severity,
    /// What kind of problem it is.
    pub category: Category,
    /// One-line headline.
    pub title: String,
    /// Why it matters.
    pub explanation: String,
    /// A replacement for the lines, when the fix is small.
    #[serde(default)]
    pub suggested_patch: Option<String>,
    /// How sure the model is.
    pub confidence: Confidence,
}

/// A checked finding with its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable within one run: `F1`, `F2`, and so on.
    pub id: String,
    /// Path of the file, as in the diff.
    pub file: String,
    /// First line of the range, on the new side of the diff.
    pub start_line: u32,
    /// Last line of the range, inclusive.
    pub end_line: u32,
    /// How serious it is.
    pub severity: Severity,
    /// What kind of problem it is.
    pub category: Category,
    /// One-line headline.
    pub title: String,
    /// Why it matters.
    pub explanation: String,
    /// A replacement for the lines, when the fix is small.
    pub suggested_patch: Option<String>,
    /// How sure the model is.
    pub confidence: Confidence,
    /// The verifier's score from 0 to 10. Absent when no verifier scored the finding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<u8>,
    /// The verifier's one-line reason for the score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_reason: Option<String>,
    /// True when a verifier was asked and gave this finding no usable score.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unverified: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// What the model is asked to return.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOutput {
    /// Short overview of the change.
    pub summary: String,
    /// Effort to review or fix, 1 (trivial) to 5 (large).
    pub effort: u8,
    /// Risk of merging.
    pub risk: Risk,
    /// Findings, each checked on its own.
    pub findings: Vec<serde_json::Value>,
    /// Verdicts on the requirements of the linked issues, when the prompt had any. Each is
    /// checked on its own.
    #[serde(default)]
    pub compliance: Vec<serde_json::Value>,
}

/// Which files the review saw. Every file given to the packer is in exactly one list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    /// Files sent to the model in full.
    pub reviewed: Vec<String>,
    /// Files sent in part because the budget ran out.
    pub clipped: Vec<String>,
    /// Files not sent at all.
    pub left_out: Vec<String>,
}

impl Coverage {
    /// Builds the coverage of `given`, the files the packer received. A file in `reviewed`
    /// or `clipped` that is not in `given` is dropped; a file in `given` that is in neither
    /// goes to `left_out`. A file named in both lists counts as reviewed.
    #[must_use]
    pub fn build(given: &[String], reviewed: &[String], clipped: &[String]) -> Self {
        let mut out = Self::default();
        for file in given {
            let list = if reviewed.contains(file) {
                &mut out.reviewed
            } else if clipped.contains(file) {
                &mut out.clipped
            } else {
                &mut out.left_out
            };
            if !list.contains(file) {
                list.push(file.clone());
            }
        }
        out
    }
}

/// Facts about the run that produced a review.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDetails {
    /// Model id that wrote the review.
    pub model: String,
    /// Characters of diff text sent to the model.
    pub input_chars: usize,
    /// Findings the model returned that were not kept: over the cap or failed a check.
    pub dropped_findings: usize,
    /// The backend asked to verify the findings, when a verifier ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier: Option<String>,
    /// Findings the verifier dropped as false or too doubtful. They are not in
    /// `dropped_findings`.
    #[serde(default)]
    pub verifier_dropped: usize,
    /// Parts the change was split into, each reviewed by its own run.
    #[serde(default)]
    pub chunks: usize,
    /// True when a part could not be reviewed: its files are in `left_out`, and the other
    /// parts' findings are all that this review has.
    #[serde(default)]
    pub partial: bool,
    /// Linked issues that were named but could not be used, each with the reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_issues: Vec<String>,
}

/// The finished review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    /// Short overview of the change.
    pub summary: String,
    /// Effort to review or fix, 1 to 5.
    pub effort: u8,
    /// Risk of merging.
    pub risk: Risk,
    /// Kept findings, most serious first.
    pub findings: Vec<Finding>,
    /// How the change meets the requirements of the linked issues. Empty when no issue was
    /// linked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compliance: Vec<ComplianceItem>,
    /// Which files the review saw.
    pub coverage: Coverage,
    /// How the run went.
    pub run: RunDetails,
}

/// The new-side line ranges of each file in the diff. A finding may point only inside them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffScope {
    ranges: BTreeMap<String, Vec<(u32, u32)>>,
}

impl DiffScope {
    /// An empty scope.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an inclusive line range of `file`. Ranges that overlap or touch become one.
    pub fn add(&mut self, file: &str, start: u32, end: u32) {
        let ranges = self.ranges.entry(file.to_owned()).or_default();
        ranges.push((start, end));
        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
        for &(s, e) in ranges.iter() {
            match merged.last_mut() {
                Some(last) if s <= last.1.saturating_add(1) => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        *ranges = merged;
    }

    /// True when every line from `start` to `end` is inside one range of `file`.
    #[must_use]
    pub fn contains(&self, file: &str, start: u32, end: u32) -> bool {
        self.ranges
            .get(file)
            .is_some_and(|ranges| ranges.iter().any(|&(s, e)| s <= start && end <= e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn only_high_and_critical_findings_block() {
        // Act and Assert
        assert!(Severity::Critical.blocks());
        assert!(Severity::High.blocks());
        assert!(!Severity::Medium.blocks());
        assert!(!Severity::Low.blocks());
        assert!(!Severity::Info.blocks());
    }

    #[test]
    fn coverage_lists_every_file_given_exactly_once() {
        // Arrange
        let given = names(&["a", "b", "c", "d"]);

        // Act
        let coverage = Coverage::build(&given, &names(&["a", "b", "zz"]), &names(&["b", "c"]));

        // Assert
        assert_eq!(coverage.reviewed, names(&["a", "b"]));
        assert_eq!(coverage.clipped, names(&["c"]));
        assert_eq!(coverage.left_out, names(&["d"]));
    }

    #[test]
    fn coverage_of_no_reviewed_files_leaves_everything_out() {
        // Arrange
        let given = names(&["a", "a", "b"]);

        // Act
        let coverage = Coverage::build(&given, &[], &[]);

        // Assert
        assert_eq!(coverage.left_out, names(&["a", "b"]));
    }

    #[test]
    fn scope_accepts_only_ranges_inside_one_span() {
        // Arrange
        let mut scope = DiffScope::new();
        scope.add("f", 5, 8);
        scope.add("f", 20, 22);

        // Act and Assert
        assert!(scope.contains("f", 5, 8));
        assert!(scope.contains("f", 21, 21));
        assert!(!scope.contains("f", 8, 20));
        assert!(!scope.contains("g", 5, 5));
    }

    #[test]
    fn touching_and_overlapping_ranges_merge() {
        // Arrange
        let mut scope = DiffScope::new();
        scope.add("f", 6, 9);
        scope.add("f", 1, 5);
        scope.add("f", 8, 12);
        scope.add("f", 20, 22);

        // Act and Assert
        assert!(scope.contains("f", 4, 12));
        assert!(scope.contains("f", 1, 12));
        assert!(!scope.contains("f", 12, 20));
        assert!(!scope.contains("f", 13, 13));
        assert!(scope.contains("f", 20, 22));
    }

    #[test]
    fn a_range_inside_another_keeps_the_outer_end() {
        // Arrange
        let mut scope = DiffScope::new();
        scope.add("f", 1, 10);
        scope.add("f", 3, 5);

        // Act and Assert
        assert!(scope.contains("f", 1, 10));
    }
}
