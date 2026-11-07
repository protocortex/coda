// SPDX-License-Identifier: Apache-2.0

//! Markdown and JSON output of a finished review.

use std::fmt::Write as _;

use crate::schema::{Finding, MAX_EFFORT, Review};
use crate::verify::MAX_SCORE;

const FENCE_CHAR: char = '`';
const MIN_FENCE: usize = 3;
const REPLACEMENT: char = '?';
const HEADING_CHAR: char = '#';
const CODE_FENCE: &str = "```";
const TILDE_FENCE: &str = "~~~";
const RULE_CHARS: [char; 4] = ['-', '=', '_', '*'];
const QUOTE_CHAR: char = '>';
const NUMBER_ENDS: [char; 2] = ['.', ')'];
const BULLET_CHARS: [char; 3] = ['-', '*', '+'];
const PARTIAL_BANNER: &str = "**Partial review:** at least one part of the change could not be \
    reviewed, so the findings below may be incomplete. The files it covered are listed under Left out.";
const ESCAPE: &str = "\\";
/// A zero-width space after `<` keeps it from opening a tag.
const ANGLE_BREAK: &str = "<\u{200b}";
/// A zero-width space after `@` keeps the host from turning the text into a mention.
const MENTION_BREAK: &str = "@\u{200b}";

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

/// Model prose for the body of the page. Line breaks stay; a line that would start a
/// heading, a rule or a code fence is escaped, also after list and quote markers, and `<` and
/// `@` cannot start HTML or a mention. The breaks are invisible zero-width spaces, so code
/// spans keep their look.
fn prose(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c != '\n' && c.is_control() { ' ' } else { c })
        .collect();
    cleaned
        .lines()
        .map(neutralize_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn neutralize_line(line: &str) -> String {
    let body = line.trim_start();
    let indent = &line[..line.len() - body.len()];
    let escape = body.starts_with(HEADING_CHAR)
        || body.starts_with(CODE_FENCE)
        || body.starts_with(TILDE_FENCE)
        || is_rule(body);
    if !escape && let Some(marker) = container_marker(body) {
        let rest = neutralize_line(&body[marker.len()..]);
        return format!("{indent}{marker}{rest}");
    }
    let lead = if escape { ESCAPE } else { "" };
    let body = body.replace('<', ANGLE_BREAK).replace('@', MENTION_BREAK);
    format!("{indent}{lead}{body}")
}

/// The list or quote marker at the start of `body`, whose content is a block of its own.
fn container_marker(body: &str) -> Option<&str> {
    if body.starts_with(QUOTE_CHAR) {
        return Some(&body[..QUOTE_CHAR.len_utf8()]);
    }
    let digits = body.bytes().take_while(u8::is_ascii_digit).count();
    let after = &body[digits..];
    let numbered = digits > 0 && after.starts_with(NUMBER_ENDS);
    let bulleted = digits == 0 && after.starts_with(BULLET_CHARS);
    let len = digits + usize::from(numbered || bulleted);
    (len > 0 && body[len..].starts_with(' ')).then(|| &body[..len])
}

/// A line of only `-`, `=` or `_` marks (with spaces) is a rule or the underline of a heading.
fn is_rule(body: &str) -> bool {
    let mut marks = body.chars().filter(|c| !c.is_whitespace());
    marks
        .next()
        .is_some_and(|first| RULE_CHARS.contains(&first) && marks.all(|c| c == first))
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

/// What the verifier said about `f`, to follow the confidence on one line.
fn verdict_note(f: &Finding) -> String {
    match (f.score, &f.verifier_reason) {
        (Some(score), Some(reason)) => {
            format!(
                ", score {score} of {MAX_SCORE}: {}",
                prose(&one_line(reason))
            )
        }
        (Some(score), None) => format!(", score {score} of {MAX_SCORE}"),
        (None, _) if f.unverified => ", unverified".to_owned(),
        (None, _) => String::new(),
    }
}

fn write_finding(out: &mut String, f: &Finding) {
    let _ = write!(
        out,
        "### {} [{}] {}: {}\n\n`{}:{}-{}`, confidence {}{}\n\n{}\n\n",
        f.id,
        f.severity.as_str(),
        f.category.as_str(),
        prose(&one_line(&f.title)),
        inline(&f.file),
        f.start_line,
        f.end_line,
        f.confidence.as_str(),
        verdict_note(f),
        prose(&f.explanation),
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
    let mut out = String::from("# Review\n\n");
    if review.run.partial {
        out.push_str(PARTIAL_BANNER);
        out.push_str("\n\n");
    }
    let _ = write!(
        out,
        "Risk: {}. Effort: {} of {MAX_EFFORT}.\n\n{}\n\n## Findings ({})\n\n",
        review.risk.as_str(),
        review.effort,
        prose(&review.summary),
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
    let _ = write!(
        out,
        "## Run\n\nModel: {}. Input: {} characters. Findings dropped: {}.",
        review.run.model, review.run.input_chars, review.run.dropped_findings,
    );
    if review.run.chunks > 1 {
        let _ = write!(out, " Chunks: {}.", review.run.chunks);
    }
    if let Some(verifier) = &review.run.verifier {
        let _ = write!(
            out,
            " Verifier: {}. Dropped by the verifier: {}.",
            inline(verifier),
            review.run.verifier_dropped
        );
    }
    out.push('\n');
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
                score: None,
                verifier_reason: None,
                unverified: false,
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
                verifier: None,
                verifier_dropped: 0,
                chunks: 1,
                partial: false,
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
    fn a_partial_review_opens_with_a_banner_and_a_whole_one_does_not() {
        // Arrange
        let whole = review();
        let mut partial = review();
        partial.run.partial = true;
        partial.run.chunks = 3;

        // Act
        let text = to_markdown(&partial);

        // Assert
        assert!(
            text.starts_with(
                "# Review\n\n**Partial review:** at least one part of the change could not be \
                 reviewed, so the findings below may be incomplete. The files it covered are \
                 listed under Left out.\n\n"
            ),
            "{text}"
        );
        assert!(text.contains("Chunks: 3."), "{text}");
        assert!(!to_markdown(&whole).contains("Partial review"));
        assert!(!to_markdown(&whole).contains("Chunks:"));
    }

    #[test]
    fn a_verified_finding_shows_its_score_and_reason_and_an_unverified_one_says_so() {
        // Arrange
        let mut r = review();
        r.findings[0].score = Some(8);
        r.findings[0].verifier_reason =
            Some("Read a.rs: the loop is off by one.\n## Fake".to_owned());
        let mut second = r.findings[0].clone();
        second.id = "F2".to_owned();
        second.score = None;
        second.verifier_reason = None;
        second.unverified = true;
        r.findings.push(second);
        r.run.verifier = Some("codex_app_server".to_owned());
        r.run.verifier_dropped = 2;

        // Act
        let text = to_markdown(&r);

        // Assert
        assert!(
            text.contains("`src/a.rs:10-12`, confidence medium, score 8 of 10: Read a.rs: the loop is off by one. ## Fake\n"),
            "{text}"
        );
        assert!(
            text.contains("`src/a.rs:10-12`, confidence medium, unverified\n"),
            "{text}"
        );
        assert!(
            text.contains("Verifier: codex_app_server. Dropped by the verifier: 2."),
            "{text}"
        );
    }

    #[test]
    fn a_review_with_no_verifier_prints_no_verifier_words() {
        // Act
        let text = to_markdown(&review());

        // Assert
        assert!(!text.contains("Verifier"));
        assert!(!text.contains("unverified"));
        assert!(!text.contains("score"));
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
    fn model_prose_cannot_forge_headings_mentions_html_or_fences() {
        // Arrange
        let hostile = "x\n\n## Coverage\n\n  # Run\nReviewed (1):\n- `fake`\n@bob <script>alert(1)</script>\n```\nsecond\n---\n===\n~~~";
        let mut r = review();
        r.summary = hostile.to_owned();
        r.findings[0].explanation = hostile.to_owned();
        r.findings[0].title = "hi @alice".to_owned();

        // Act
        let text = to_markdown(&r);

        // Assert
        assert_eq!(text.matches("\n## Coverage\n").count(), 1, "{text}");
        assert_eq!(text.matches("\n## Run\n").count(), 1, "{text}");
        assert!(!text.contains("@bob"));
        assert!(!text.contains("@alice"));
        assert!(!text.contains("<script>"));
        for line in text.lines() {
            assert!(
                !line.trim_start().starts_with("```") || line == "```",
                "{line}"
            );
        }
        assert!(!text.contains("\n---\n"));
        assert!(!text.contains("\n===\n"));
        assert!(!text.contains("\n~~~"));
        assert!(text.contains("second"));
    }

    #[test]
    fn headings_and_fences_nested_in_lists_or_quotes_are_escaped() {
        // Arrange
        let mut r = review();
        r.findings[0].explanation =
            "- # Coverage\n1. ## Run\n> # Findings (0)\n- > ```\n***\n* * *\n  + ~~~".to_owned();

        // Act
        let text = to_markdown(&r);

        // Assert
        for bad in [
            "- # ",
            "1. ## ",
            "> # ",
            "- > ```",
            "\n***\n",
            "\n* * *\n",
            "+ ~~~",
        ] {
            assert!(!text.contains(bad), "{bad}\n{text}");
        }
        assert!(text.contains("- \\# Coverage"));
    }

    #[test]
    fn plain_prose_is_left_as_written() {
        // Arrange
        let mut r = review();
        r.findings[0].explanation = "Use a < b and `x`, then c.\n\nSecond paragraph.".to_owned();

        // Act
        let text = to_markdown(&r);

        // Assert
        assert!(text.contains("Use a <\u{200b} b and `x`, then c.\n\nSecond paragraph.\n"));
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
