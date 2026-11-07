// SPDX-License-Identifier: Apache-2.0

//! Splits a change that does not fit one run into parts and merges what the parts report.
//!
//! The merge is conservative: nothing a part found is lost, and no number is made kinder
//! than a part's own. The risk and the effort are the highest of the parts, a finding that
//! two parts both made keeps the worst severity and the lowest confidence, and a finding
//! about security is never cut by the finding cap.

use crate::diff::FileDiff;
use crate::pack::{OmitReason, PackOptions, Packed, pack};
use crate::parse::Parsed;
use crate::schema::{Category, ComplianceItem, Coverage, Finding, MIN_EFFORT, Risk};

/// Splits `files` into at most `max_chunks` packed parts, each within the budget.
///
/// The first part is packed as a lone run would be. The files the budget cut go to the next
/// part, and so on until none are left or `max_chunks` parts exist; files still left then
/// are named in the coverage as left out. A part that fits none of its files ends the
/// split, since another try would give the same part.
#[must_use]
pub fn plan(files: &[FileDiff], options: PackOptions<'_>, max_chunks: usize) -> Vec<Packed> {
    let mut chunks: Vec<Packed> = Vec::new();
    let mut rest: Vec<FileDiff> = files.to_vec();
    while chunks.len() < max_chunks.max(1) {
        let packed = pack(&rest, options);
        let made_progress =
            !packed.coverage.reviewed.is_empty() || !packed.coverage.clipped.is_empty();
        if !made_progress {
            if chunks.is_empty() {
                chunks.push(packed);
            }
            break;
        }
        let cut: Vec<&str> = packed
            .omitted
            .iter()
            .filter(|o| o.reason == OmitReason::OverBudget)
            .map(|o| o.path.as_str())
            .collect();
        rest.retain(|file| cut.contains(&file.path.as_str()));
        chunks.push(packed);
        if rest.is_empty() {
            break;
        }
    }
    chunks
}

fn reviewed_of<'a>(chunks: impl Iterator<Item = &'a Packed>) -> Vec<String> {
    chunks.flat_map(|c| c.coverage.reviewed.clone()).collect()
}

fn clipped_of<'a>(chunks: impl Iterator<Item = &'a Packed>) -> Vec<String> {
    chunks.flat_map(|c| c.coverage.clipped.clone()).collect()
}

/// What one part came to: the part and what the reviewer made of it, or nothing when the
/// run failed.
pub struct Outcome<'a> {
    /// The part that was reviewed.
    pub packed: &'a Packed,
    /// The checked answer, or `None` when the run failed after its retry.
    pub parsed: Option<Parsed>,
}

/// The parts merged into one review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// The merged answer.
    pub parsed: Parsed,
    /// Which of the given files were reviewed: the files of a part that failed are left out.
    pub coverage: Coverage,
    /// How many parts there were.
    pub chunks: usize,
    /// True when a part failed.
    pub partial: bool,
}

fn same_finding(a: &Finding, b: &Finding) -> bool {
    a.file == b.file
        && a.start_line == b.start_line
        && a.end_line == b.end_line
        && a.title == b.title
        && a.explanation == b.explanation
}

fn absorb(kept: &mut Finding, other: &Finding) {
    kept.severity = kept.severity.min(other.severity);
    kept.confidence = kept.confidence.min(other.confidence);
    if other.category == Category::Security {
        kept.category = Category::Security;
    }
}

/// Merges the parts of one change. `given` is every file the review was asked about, and
/// `max_findings` caps the findings that are not about security, which all stay.
#[must_use]
pub fn merge(given: &[String], outcomes: Vec<Outcome<'_>>, max_findings: usize) -> Merged {
    let chunks = outcomes.len();
    let partial = outcomes.iter().any(|o| o.parsed.is_none());
    let ok: Vec<&Packed> = outcomes
        .iter()
        .filter(|o| o.parsed.is_some())
        .map(|o| o.packed)
        .collect();
    let coverage = Coverage::build(
        given,
        &reviewed_of(ok.iter().copied()),
        &clipped_of(ok.iter().copied()),
    );

    let mut summaries: Vec<String> = Vec::new();
    let mut effort = MIN_EFFORT;
    let mut risk = Risk::Low;
    let mut found: Vec<Finding> = Vec::new();
    let mut rejected = Vec::new();
    let mut over_cap = 0;
    let mut compliance: Vec<ComplianceItem> = Vec::new();
    for parsed in outcomes.into_iter().filter_map(|o| o.parsed) {
        if !parsed.summary.trim().is_empty() && !summaries.contains(&parsed.summary) {
            summaries.push(parsed.summary);
        }
        effort = effort.max(parsed.effort);
        risk = risk.max(parsed.risk);
        rejected.extend(parsed.rejected);
        over_cap += parsed.over_cap;
        for item in parsed.compliance {
            match compliance
                .iter_mut()
                .find(|kept| kept.issue == item.issue && kept.requirement == item.requirement)
            {
                Some(kept) => kept.verdict = kept.verdict.max(item.verdict),
                None => compliance.push(item),
            }
        }
        for finding in parsed.findings {
            match found.iter_mut().find(|kept| same_finding(kept, &finding)) {
                Some(kept) => absorb(kept, &finding),
                None => found.push(finding),
            }
        }
    }

    found.sort_by_key(|f| f.severity);
    let total = found.len();
    let mut plain = 0;
    let mut findings: Vec<Finding> = found
        .into_iter()
        .filter(|f| {
            if f.category == Category::Security {
                return true;
            }
            plain += 1;
            plain <= max_findings
        })
        .collect();
    over_cap += total - findings.len();
    for (n, finding) in findings.iter_mut().enumerate() {
        finding.id = format!("F{}", n + 1);
    }

    Merged {
        parsed: Parsed {
            summary: summaries.join(" "),
            effort,
            risk,
            findings,
            rejected,
            over_cap,
            compliance,
        },
        coverage,
        chunks,
        partial,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_diff;
    use crate::ignore::IgnoreSet;
    use crate::parse::Rejection;
    use crate::schema::{Confidence, Severity, Verdict};

    const BIG_BUDGET: usize = 100_000;

    fn plan_coverage(files: &[FileDiff], chunks: &[Packed]) -> Coverage {
        let given: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        Coverage::build(
            &given,
            &reviewed_of(chunks.iter()),
            &clipped_of(chunks.iter()),
        )
    }

    fn file_diff(path: &str, lines: usize) -> String {
        let body = "+a line\n".repeat(lines);
        format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -0,0 +1,{lines} @@\n{body}"
        )
    }

    fn files(spec: &[(&str, usize)]) -> Vec<FileDiff> {
        let text: String = spec.iter().map(|(p, n)| file_diff(p, *n)).collect();
        parse_diff(&text)
    }

    fn options(ignore: &IgnoreSet, budget: usize) -> PackOptions<'_> {
        PackOptions {
            budget,
            ignore,
            main_extension: None,
        }
    }

    fn no_ignore() -> IgnoreSet {
        IgnoreSet::new(&[], false).unwrap()
    }

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_requirement_keeps_the_best_verdict_any_part_gave_it() {
        // Arrange
        let item = |verdict| ComplianceItem {
            issue: "#1".to_owned(),
            requirement: "Retry three times".to_owned(),
            verdict,
        };
        let a = packed(&["a.rs"]);
        let b = packed(&["b.rs"]);
        let mut first = parsed(Risk::Low, 1, vec![]);
        first.compliance = vec![item(Verdict::Not)];
        let mut second = parsed(Risk::Low, 1, vec![]);
        second.compliance = vec![item(Verdict::Fully)];
        let outcomes = vec![
            Outcome {
                packed: &a,
                parsed: Some(first),
            },
            Outcome {
                packed: &b,
                parsed: Some(second),
            },
        ];

        // Act
        let merged = merge(&["a.rs".to_owned(), "b.rs".to_owned()], outcomes, 5);

        // Assert
        assert_eq!(merged.parsed.compliance.len(), 1);
        assert_eq!(merged.parsed.compliance[0].verdict, Verdict::Fully);
    }

    #[test]
    fn a_change_that_fits_is_one_part() {
        // Arrange
        let files = files(&[("a.rs", 3), ("b.rs", 3)]);
        let ignore = no_ignore();

        // Act
        let chunks = plan(&files, options(&ignore, BIG_BUDGET), 4);

        // Assert
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].coverage.reviewed, names(&["a.rs", "b.rs"]));
    }

    #[test]
    fn files_the_budget_cut_go_to_the_next_part() {
        // Arrange
        let files = files(&[("a.rs", 5), ("b.rs", 5), ("c.rs", 5)]);
        let ignore = no_ignore();
        let one_file = pack(&files[..1], options(&ignore, BIG_BUDGET))
            .text
            .chars()
            .count();

        // Act
        let chunks = plan(&files, options(&ignore, one_file + 5), 4);

        // Assert
        assert_eq!(chunks.len(), 3);
        for (chunk, name) in chunks.iter().zip(["a.rs", "b.rs", "c.rs"]) {
            assert_eq!(chunks_reviewed(chunk), names(&[name]));
        }
        assert_eq!(plan_coverage(&files, &chunks).left_out, names(&[]));
    }

    fn chunks_reviewed(chunk: &Packed) -> Vec<String> {
        chunk.coverage.reviewed.clone()
    }

    #[test]
    fn the_number_of_parts_is_capped_and_the_rest_is_left_out() {
        // Arrange
        let files = files(&[("a.rs", 5), ("b.rs", 5), ("c.rs", 5), ("d.rs", 5)]);
        let ignore = no_ignore();
        let one_file = pack(&files[..1], options(&ignore, BIG_BUDGET))
            .text
            .chars()
            .count();

        // Act
        let chunks = plan(&files, options(&ignore, one_file + 5), 2);

        // Assert
        assert_eq!(chunks.len(), 2);
        let coverage = plan_coverage(&files, &chunks);
        assert_eq!(coverage.reviewed, names(&["a.rs", "b.rs"]));
        assert_eq!(coverage.left_out, names(&["c.rs", "d.rs"]));
    }

    #[test]
    fn a_budget_that_fits_no_file_makes_one_empty_part_not_a_loop() {
        // Arrange
        let files = files(&[("a.rs", 5)]);
        let ignore = no_ignore();

        // Act
        let chunks = plan(&files, options(&ignore, 3), 4);

        // Assert
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].coverage.reviewed, names(&[]));
        assert_eq!(chunks[0].coverage.clipped, names(&[]));
    }

    #[test]
    fn files_that_are_not_over_budget_are_not_carried_into_later_parts() {
        // Arrange
        let mut files = files(&[("a.rs", 5), ("b.rs", 5)]);
        files.extend(parse_diff(&file_diff("Cargo.lock", 2)));
        let ignore = IgnoreSet::new(&[], true).unwrap();
        let one_file = pack(&files[..1], options(&ignore, BIG_BUDGET))
            .text
            .chars()
            .count();

        // Act
        let chunks = plan(&files, options(&ignore, one_file + 5), 4);

        // Assert
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            plan_coverage(&files, &chunks).left_out,
            names(&["Cargo.lock"])
        );
        assert_eq!(chunks[1].omitted, vec![]);
    }

    fn finding(file: &str, line: u32, severity: Severity) -> Finding {
        Finding {
            id: "F9".to_owned(),
            file: file.to_owned(),
            start_line: line,
            end_line: line,
            severity,
            category: Category::Bug,
            title: "t".to_owned(),
            explanation: "e".to_owned(),
            suggested_patch: None,
            confidence: Confidence::High,
            score: None,
            verifier_reason: None,
            unverified: false,
        }
    }

    fn parsed(risk: Risk, effort: u8, findings: Vec<Finding>) -> Parsed {
        Parsed {
            summary: format!("{risk:?}"),
            effort,
            risk,
            findings,
            rejected: vec![],
            over_cap: 0,
            compliance: vec![],
        }
    }

    fn packed(reviewed: &[&str]) -> Packed {
        Packed {
            text: String::new(),
            scope: crate::schema::DiffScope::new(),
            coverage: Coverage {
                reviewed: names(reviewed),
                clipped: vec![],
                left_out: vec![],
            },
            omitted: vec![],
        }
    }

    fn merged_of(parts: &[(Packed, Option<Parsed>)], max_findings: usize) -> Merged {
        let given: Vec<String> = parts
            .iter()
            .flat_map(|(p, _)| p.coverage.reviewed.clone())
            .collect();
        let outcomes = parts
            .iter()
            .map(|(packed, parsed)| Outcome {
                packed,
                parsed: parsed.clone(),
            })
            .collect();
        merge(&given, outcomes, max_findings)
    }

    #[test]
    fn the_merged_risk_and_effort_are_the_highest_of_the_parts() {
        // Arrange
        let parts = vec![
            (packed(&["a"]), Some(parsed(Risk::Medium, 2, vec![]))),
            (packed(&["b"]), Some(parsed(Risk::High, 4, vec![]))),
            (packed(&["c"]), Some(parsed(Risk::Low, 1, vec![]))),
        ];

        // Act
        let merged = merged_of(&parts, 5);

        // Assert
        assert_eq!(merged.parsed.risk, Risk::High);
        assert_eq!(merged.parsed.effort, 4);
        assert!(!merged.partial);
        assert_eq!(merged.chunks, 3);
    }

    #[test]
    fn the_merged_risk_is_never_lower_than_a_part_in_any_order() {
        for order in [
            [Risk::High, Risk::Low, Risk::Medium],
            [Risk::Low, Risk::Medium, Risk::High],
            [Risk::Medium, Risk::High, Risk::Low],
        ] {
            // Arrange
            let parts: Vec<_> = order
                .iter()
                .enumerate()
                .map(|(n, risk)| (packed(&[&n.to_string()]), Some(parsed(*risk, 1, vec![]))))
                .collect();

            // Act
            let merged = merged_of(&parts, 5);

            // Assert
            assert_eq!(merged.parsed.risk, Risk::High, "{order:?}");
        }
    }

    #[test]
    fn findings_of_all_parts_are_kept_and_numbered_again_most_serious_first() {
        // Arrange
        let parts = vec![
            (
                packed(&["a"]),
                Some(parsed(Risk::Low, 1, vec![finding("a", 1, Severity::Low)])),
            ),
            (
                packed(&["b"]),
                Some(parsed(
                    Risk::Low,
                    1,
                    vec![finding("b", 2, Severity::Critical)],
                )),
            ),
        ];

        // Act
        let merged = merged_of(&parts, 5);

        // Assert
        let ids: Vec<(&str, &str)> = merged
            .parsed
            .findings
            .iter()
            .map(|f| (f.id.as_str(), f.file.as_str()))
            .collect();
        assert_eq!(ids, [("F1", "b"), ("F2", "a")]);
    }

    #[test]
    fn a_finding_two_parts_made_is_kept_once_with_the_worst_severity_and_lowest_confidence() {
        // Arrange
        let mut first = finding("a", 3, Severity::Low);
        first.confidence = Confidence::High;
        let mut second = finding("a", 3, Severity::High);
        second.confidence = Confidence::Low;
        let parts = vec![
            (packed(&["a"]), Some(parsed(Risk::Low, 1, vec![first]))),
            (packed(&["b"]), Some(parsed(Risk::Low, 1, vec![second]))),
        ];

        // Act
        let merged = merged_of(&parts, 5);

        // Assert
        assert_eq!(merged.parsed.findings.len(), 1);
        assert_eq!(merged.parsed.findings[0].severity, Severity::High);
        assert_eq!(merged.parsed.findings[0].confidence, Confidence::Low);
    }

    #[test]
    fn findings_that_differ_in_line_or_text_are_not_duplicates() {
        // Arrange
        let mut other_text = finding("a", 3, Severity::Low);
        other_text.title = "other".to_owned();
        let parts = vec![(
            packed(&["a"]),
            Some(parsed(
                Risk::Low,
                1,
                vec![
                    finding("a", 3, Severity::Low),
                    finding("a", 4, Severity::Low),
                    other_text,
                ],
            )),
        )];

        // Act
        let merged = merged_of(&parts, 5);

        // Assert
        assert_eq!(merged.parsed.findings.len(), 3);
    }

    #[test]
    fn the_cap_drops_the_least_serious_but_never_a_security_finding() {
        // Arrange
        let mut security = finding("a", 9, Severity::Info);
        security.category = Category::Security;
        let parts = vec![(
            packed(&["a"]),
            Some(parsed(
                Risk::Low,
                1,
                vec![
                    finding("a", 1, Severity::Critical),
                    finding("a", 2, Severity::High),
                    finding("a", 3, Severity::Medium),
                    security,
                ],
            )),
        )];

        // Act
        let merged = merged_of(&parts, 2);

        // Assert
        let lines: Vec<u32> = merged
            .parsed
            .findings
            .iter()
            .map(|f| f.start_line)
            .collect();
        assert_eq!(lines, [1, 2, 9]);
        assert_eq!(merged.parsed.over_cap, 1);
    }

    #[test]
    fn a_security_finding_that_sorts_first_does_not_use_a_slot_of_the_cap() {
        // Arrange
        let mut security = finding("a", 9, Severity::Critical);
        security.category = Category::Security;
        let parts = vec![(
            packed(&["a"]),
            Some(parsed(
                Risk::Low,
                1,
                vec![
                    security,
                    finding("a", 1, Severity::High),
                    finding("a", 2, Severity::Medium),
                    finding("a", 3, Severity::Low),
                ],
            )),
        )];

        // Act
        let merged = merged_of(&parts, 2);

        // Assert
        let lines: Vec<u32> = merged
            .parsed
            .findings
            .iter()
            .map(|f| f.start_line)
            .collect();
        assert_eq!(lines, [9, 1, 2]);
        assert_eq!(merged.parsed.over_cap, 1);
    }

    #[test]
    fn a_duplicate_that_is_security_in_one_part_stays_security() {
        // Arrange
        let plain = finding("a", 3, Severity::Low);
        let mut security = finding("a", 3, Severity::Low);
        security.category = Category::Security;
        let parts = vec![
            (packed(&["a"]), Some(parsed(Risk::Low, 1, vec![plain]))),
            (packed(&["b"]), Some(parsed(Risk::Low, 1, vec![security]))),
        ];

        // Act
        let merged = merged_of(&parts, 5);

        // Assert
        assert_eq!(merged.parsed.findings[0].category, Category::Security);
    }

    #[test]
    fn a_failed_part_keeps_the_other_findings_marks_the_review_partial_and_leaves_its_files_out() {
        // Arrange
        let parts = vec![
            (
                packed(&["a"]),
                Some(parsed(
                    Risk::Medium,
                    2,
                    vec![finding("a", 1, Severity::High)],
                )),
            ),
            (packed(&["b", "c"]), None),
        ];

        // Act
        let merged = merged_of(&parts, 5);

        // Assert
        assert!(merged.partial);
        assert_eq!(merged.parsed.findings.len(), 1);
        assert_eq!(merged.coverage.reviewed, names(&["a"]));
        assert_eq!(merged.coverage.left_out, names(&["b", "c"]));
        assert_eq!(merged.parsed.risk, Risk::Medium);
    }

    #[test]
    fn summaries_are_joined_once_each_and_rejections_and_caps_add_up() {
        // Arrange
        let mut one = parsed(Risk::Low, 1, vec![]);
        one.summary = "Same.".to_owned();
        one.over_cap = 2;
        one.rejected = vec![Rejection {
            index: 0,
            reason: "r".to_owned(),
        }];
        let mut two = one.clone();
        two.over_cap = 1;
        let mut three = parsed(Risk::Low, 1, vec![]);
        three.summary = "Other.".to_owned();
        let parts = vec![
            (packed(&["a"]), Some(one)),
            (packed(&["b"]), Some(two)),
            (packed(&["c"]), Some(three)),
        ];

        // Act
        let merged = merged_of(&parts, 5);

        // Assert
        assert_eq!(merged.parsed.summary, "Same. Other.");
        assert_eq!(merged.parsed.over_cap, 3);
        assert_eq!(merged.parsed.rejected.len(), 2);
    }
}
