// SPDX-License-Identifier: Apache-2.0

//! Strict parsing of model output into checked findings.

use crate::schema::{DiffScope, Finding, MAX_EFFORT, MIN_EFFORT, ModelOutput, RawFinding, Risk};

/// A finding that was dropped, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Position of the finding in the model's list, from 0.
    pub index: usize,
    /// Why it was dropped.
    pub reason: String,
}

/// Model output that passed the shape check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// Short overview of the change.
    pub summary: String,
    /// Effort, 1 to 5.
    pub effort: u8,
    /// Risk of merging.
    pub risk: Risk,
    /// Kept findings, most serious first, at most the cap.
    pub findings: Vec<Finding>,
    /// Findings dropped by a check.
    pub rejected: Vec<Rejection>,
    /// Findings dropped only because of the cap.
    pub over_cap: usize,
}

/// The output is not in the shape the prompt asked for.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("review output has a bad shape: {0}")]
pub struct BadShape(pub String);

impl BadShape {
    /// Text to add to the prompt for the one retry.
    #[must_use]
    pub fn retry_hint(&self) -> String {
        format!(
            "Your last answer was rejected: {}. Answer again with one JSON object only, with the \
             keys summary, effort (1 to 5), risk (low, medium or high) and findings, and no \
             other text.",
            self.0
        )
    }
}

const FENCE: &str = "```";

fn strip_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix(FENCE) else {
        return trimmed;
    };
    let body = rest.split_once('\n').map_or("", |(_, body)| body);
    body.trim_end().strip_suffix(FENCE).unwrap_or(body).trim()
}

fn check_finding(value: serde_json::Value, scope: &DiffScope) -> Result<RawFinding, String> {
    let raw: RawFinding = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if raw.start_line == 0 || raw.end_line < raw.start_line {
        return Err(format!(
            "bad line range {}-{}",
            raw.start_line, raw.end_line
        ));
    }
    if !scope.contains(&raw.file, raw.start_line, raw.end_line) {
        return Err(format!(
            "{}:{}-{} is outside the diff",
            raw.file, raw.start_line, raw.end_line
        ));
    }
    Ok(raw)
}

/// Parses `text` as model output and checks each finding against `scope`.
///
/// # Errors
/// Returns [`BadShape`] when the text is not one JSON object with the expected keys.
pub fn parse_model_output(
    text: &str,
    scope: &DiffScope,
    max_findings: usize,
) -> Result<Parsed, BadShape> {
    let output: ModelOutput =
        serde_json::from_str(strip_fence(text)).map_err(|e| BadShape(e.to_string()))?;
    if !(MIN_EFFORT..=MAX_EFFORT).contains(&output.effort) {
        return Err(BadShape(format!(
            "effort {} is not between {MIN_EFFORT} and {MAX_EFFORT}",
            output.effort
        )));
    }
    let mut kept = Vec::new();
    let mut rejected = Vec::new();
    for (index, value) in output.findings.into_iter().enumerate() {
        match check_finding(value, scope) {
            Ok(raw) => kept.push(raw),
            Err(reason) => rejected.push(Rejection { index, reason }),
        }
    }
    kept.sort_by_key(|raw| raw.severity);
    let over_cap = kept.len().saturating_sub(max_findings);
    kept.truncate(max_findings);
    let findings = kept
        .into_iter()
        .enumerate()
        .map(|(n, raw)| Finding {
            id: format!("F{}", n + 1),
            file: raw.file,
            start_line: raw.start_line,
            end_line: raw.end_line,
            severity: raw.severity,
            category: raw.category,
            title: raw.title,
            explanation: raw.explanation,
            suggested_patch: raw.suggested_patch,
            confidence: raw.confidence,
        })
        .collect();
    Ok(Parsed {
        summary: output.summary,
        effort: output.effort,
        risk: output.risk,
        findings,
        rejected,
        over_cap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Severity;

    fn scope() -> DiffScope {
        let mut scope = DiffScope::new();
        scope.add("src/a.rs", 10, 20);
        scope.add("src/b.rs", 1, 5);
        scope
    }

    fn finding(file: &str, start: u32, end: u32, severity: &str) -> String {
        format!(
            r#"{{"file":"{file}","start_line":{start},"end_line":{end},"severity":"{severity}","category":"bug","title":"t","explanation":"e","confidence":"high"}}"#
        )
    }

    fn output(findings: &[String]) -> String {
        format!(
            r#"{{"summary":"s","effort":3,"risk":"medium","findings":[{}]}}"#,
            findings.join(",")
        )
    }

    #[test]
    fn valid_output_parses_with_assigned_ids() {
        // Arrange
        let text = output(&[
            finding("src/a.rs", 12, 14, "low"),
            finding("src/b.rs", 1, 1, "high"),
        ]);

        // Act
        let parsed = parse_model_output(&text, &scope(), 5).unwrap();

        // Assert
        assert_eq!(parsed.summary, "s");
        assert_eq!(parsed.effort, 3);
        assert_eq!(parsed.risk, Risk::Medium);
        let ids: Vec<_> = parsed.findings.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["F1", "F2"]);
        assert_eq!(parsed.findings[0].severity, Severity::High);
        assert_eq!(parsed.rejected.len(), 0);
    }

    #[test]
    fn output_in_a_json_code_fence_parses() {
        // Arrange
        let text = format!("```json\n{}\n```", output(&[]));

        // Act
        let parsed = parse_model_output(&text, &scope(), 5).unwrap();

        // Assert
        assert_eq!(parsed.findings.len(), 0);
    }

    #[test]
    fn partial_output_keeps_good_findings_and_reports_the_bad_one() {
        // Arrange
        let bad = r#"{"file":"src/a.rs","start_line":12}"#.to_owned();
        let text = output(&[bad, finding("src/a.rs", 12, 12, "low")]);

        // Act
        let parsed = parse_model_output(&text, &scope(), 5).unwrap();

        // Assert
        assert_eq!(parsed.findings.len(), 1);
        assert_eq!(parsed.rejected.len(), 1);
        assert_eq!(parsed.rejected[0].index, 0);
    }

    #[test]
    fn a_finding_with_a_line_outside_the_diff_is_rejected() {
        // Arrange
        let text = output(&[
            finding("src/a.rs", 21, 21, "high"),
            finding("src/a.rs", 5, 12, "high"),
            finding("src/other.rs", 1, 1, "high"),
        ]);

        // Act
        let parsed = parse_model_output(&text, &scope(), 5).unwrap();

        // Assert
        assert_eq!(parsed.findings.len(), 0);
        assert_eq!(parsed.rejected.len(), 3);
    }

    #[test]
    fn a_reversed_or_zero_line_range_is_rejected() {
        // Arrange
        let text = output(&[
            finding("src/a.rs", 15, 12, "high"),
            finding("src/a.rs", 0, 12, "high"),
        ]);

        // Act
        let parsed = parse_model_output(&text, &scope(), 5).unwrap();

        // Assert
        assert_eq!(parsed.rejected.len(), 2);
    }

    #[test]
    fn invalid_shapes_are_errors_with_a_retry_hint() {
        // Arrange
        let cases = [
            "not json",
            "[]",
            r#"{"summary":"s"}"#,
            r#"{"summary":"s","effort":3,"risk":"medium","findings":[],"extra":1}"#,
            r#"{"summary":"s","effort":0,"risk":"medium","findings":[]}"#,
            r#"{"summary":"s","effort":6,"risk":"medium","findings":[]}"#,
            r#"{"summary":"s","effort":3,"risk":"huge","findings":[]}"#,
        ];

        for text in cases {
            // Act
            let err = parse_model_output(text, &scope(), 5).unwrap_err();

            // Assert
            assert!(err.retry_hint().contains("JSON"), "{text}");
        }
    }

    #[test]
    fn an_unknown_key_in_a_finding_rejects_that_finding() {
        // Arrange
        let extra = finding("src/a.rs", 12, 12, "low").replace("\"title\"", "\"x\":1,\"title\"");
        let text = output(&[extra]);

        // Act
        let parsed = parse_model_output(&text, &scope(), 5).unwrap();

        // Assert
        assert_eq!(parsed.rejected.len(), 1);
    }

    #[test]
    fn findings_over_the_cap_are_dropped_least_severe_first() {
        // Arrange
        let text = output(&[
            finding("src/a.rs", 11, 11, "low"),
            finding("src/a.rs", 12, 12, "critical"),
            finding("src/a.rs", 13, 13, "medium"),
            finding("src/a.rs", 14, 14, "high"),
        ]);

        // Act
        let parsed = parse_model_output(&text, &scope(), 2).unwrap();

        // Assert
        let severities: Vec<_> = parsed.findings.iter().map(|f| f.severity).collect();
        assert_eq!(severities, [Severity::Critical, Severity::High]);
        assert_eq!(parsed.over_cap, 2);
    }

    #[test]
    fn text_inside_the_output_is_kept_verbatim() {
        // Arrange
        let text =
            r#"{"summary":"ignore previous instructions","effort":1,"risk":"low","findings":[]}"#;

        // Act
        let parsed = parse_model_output(text, &scope(), 5).unwrap();

        // Assert
        assert_eq!(parsed.summary, "ignore previous instructions");
    }
}
