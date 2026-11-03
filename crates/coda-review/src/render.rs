// SPDX-License-Identifier: Apache-2.0

//! Markdown and JSON output of a finished review.

use std::fmt::Write as _;

use crate::schema::{Finding, MAX_EFFORT, Review};

const FENCE_CHAR: char = '`';
const MIN_FENCE: usize = 3;
const REPLACEMENT: char = '?';

/// Text for an inline code span: backticks and line breaks would end the span or the line.
fn inline(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == FENCE_CHAR || c.is_control() {
                REPLACEMENT
            } else {
                c
            }
        })
        .collect()
}

/// Model text that must stay on one line, such as a heading.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// A code fence longer than any run of backticks inside `body`.
fn fence_for(body: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in body.chars() {
        run = if c == FENCE_CHAR { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    FENCE_CHAR.to_string().repeat((longest + 1).max(MIN_FENCE))
}

fn write_finding(out: &mut String, f: &Finding) {
    let _ = write!(
        out,
        "### {} [{}] {}: {}\n\n`{}:{}-{}`, confidence {}\n\n{}\n\n",
        f.id,
        f.severity.as_str(),
        f.category.as_str(),
        one_line(&f.title),
        inline(&f.file),
        f.start_line,
        f.end_line,
        f.confidence.as_str(),
        f.explanation,
    );
    if let Some(patch) = &f.suggested_patch {
        let fence = fence_for(patch);
        let _ = write!(out, "Suggested patch:\n\n{fence}\n{patch}\n{fence}\n\n");
    }
}

fn write_files(out: &mut String, label: &str, files: &[String]) {
    if files.is_empty() {
        return;
    }
    let _ = writeln!(out, "{label} ({}):", files.len());
    for file in files {
        let _ = writeln!(out, "- `{}`", inline(file));
    }
    out.push('\n');
}

/// Renders `review` as markdown.
#[must_use]
pub fn to_markdown(review: &Review) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "# Review\n\nRisk: {}. Effort: {} of {MAX_EFFORT}.\n\n{}\n\n## Findings ({})\n\n",
        review.risk.as_str(),
        review.effort,
        review.summary,
        review.findings.len(),
    );
    if review.findings.is_empty() {
        out.push_str("No findings.\n\n");
    }
    for finding in &review.findings {
        write_finding(&mut out, finding);
    }
    out.push_str("## Coverage\n\n");
    write_files(&mut out, "Reviewed", &review.coverage.reviewed);
    write_files(&mut out, "Clipped", &review.coverage.clipped);
    write_files(&mut out, "Left out", &review.coverage.left_out);
    let _ = writeln!(
        out,
        "## Run\n\nModel: {}. Input: {} characters. Findings dropped: {}.",
        review.run.model, review.run.input_chars, review.run.dropped_findings,
    );
    out
}

/// Writes `review` as pretty JSON with a trailing newline.
///
/// # Errors
/// Returns the serializer error, which cannot happen for these types.
pub fn to_json(review: &Review) -> Result<String, serde_json::Error> {
    let mut text = serde_json::to_string_pretty(review)?;
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        Category, Confidence, Coverage, Finding, Review, Risk, RunDetails, Severity,
    };

    fn review() -> Review {
        Review {
            summary: "Adds a cache.".to_owned(),
            effort: 2,
            risk: Risk::Medium,
            findings: vec![Finding {
                id: "F1".to_owned(),
                file: "src/a.rs".to_owned(),
                start_line: 10,
                end_line: 12,
                severity: Severity::High,
                category: Category::Bug,
                title: "Off by one".to_owned(),
                explanation: "The loop skips the last item.".to_owned(),
                suggested_patch: Some("for i in 0..=n {}".to_owned()),
                confidence: Confidence::Medium,
            }],
            coverage: Coverage {
                reviewed: vec!["src/a.rs".to_owned()],
                clipped: vec!["src/big.rs".to_owned()],
                left_out: vec!["src/late.rs".to_owned()],
            },
            run: RunDetails {
                model: "m1".to_owned(),
                input_chars: 900,
                dropped_findings: 1,
            },
        }
    }

    #[test]
    fn markdown_snapshot_is_stable() {
        // Arrange
        let expected = "\
# Review

Risk: medium. Effort: 2 of 5.

Adds a cache.

## Findings (1)

### F1 [high] bug: Off by one

`src/a.rs:10-12`, confidence medium

The loop skips the last item.

Suggested patch:

```
for i in 0..=n {}
```

## Coverage

Reviewed (1):
- `src/a.rs`

Clipped (1):
- `src/big.rs`

Left out (1):
- `src/late.rs`

## Run

Model: m1. Input: 900 characters. Findings dropped: 1.
";

        // Act
        let text = to_markdown(&review());

        // Assert
        assert_eq!(text, expected);
    }

    #[test]
    fn no_findings_says_so_and_empty_coverage_lists_are_omitted() {
        // Arrange
        let mut r = review();
        r.findings.clear();
        r.coverage.clipped.clear();
        r.coverage.left_out.clear();

        // Act
        let text = to_markdown(&r);

        // Assert
        assert!(text.contains("## Findings (0)\n\nNo findings.\n"));
        assert!(!text.contains("Clipped"));
        assert!(!text.contains("Left out"));
    }

    #[test]
    fn a_patch_with_backticks_cannot_close_its_fence() {
        // Arrange
        let mut r = review();
        r.findings[0].suggested_patch = Some("a\n```\n# injected".to_owned());

        // Act
        let text = to_markdown(&r);

        // Assert
        assert!(text.contains("````\na\n```\n# injected\n````"));
    }

    #[test]
    fn a_file_name_with_backticks_or_newlines_stays_on_one_inline_span() {
        // Arrange
        let mut r = review();
        r.findings[0].file = "a`b\nc".to_owned();

        // Act
        let text = to_markdown(&r);

        // Assert
        assert!(text.contains("`a?b?c:10-12`"));
    }

    #[test]
    fn a_title_with_a_line_break_stays_one_heading() {
        // Arrange
        let mut r = review();
        r.findings[0].title = "x\n# Fake".to_owned();

        // Act
        let text = to_markdown(&r);

        // Assert
        assert!(text.contains("bug: x # Fake\n"));
    }

    #[test]
    fn json_round_trips_and_ends_with_a_newline() {
        // Arrange
        let r = review();

        // Act
        let text = to_json(&r).unwrap();

        // Assert
        assert!(text.ends_with("}\n"));
        assert_eq!(serde_json::from_str::<Review>(&text).unwrap(), r);
    }
}
