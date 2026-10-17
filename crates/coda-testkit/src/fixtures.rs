// SPDX-License-Identifier: AGPL-3.0-only

//! Recorded Claude Code transcripts and the rules for trusting them.
//!
//! A transcript is a JSONL file whose first line is a header naming its
//! label. Only a `live` transcript, with the claude version, recording time
//! and recorder id filled in, may whitelist `control_response` shapes. A
//! `synthetic` one is hand-written and proves nothing about the real protocol.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/claude");
const TRANSCRIPT_EXTENSION: &str = "jsonl";
const HEADER_TYPE: &str = "coda_recording";
const CONTROL_RESPONSE_TYPE: &str = "control_response";
const LABEL_LIVE: &str = "live";
const LABEL_SYNTHETIC: &str = "synthetic";
const FIELD_VERSION: &str = "claude_version";
const FIELD_RECORDED_AT: &str = "recorded_at";
const FIELD_RECORDER: &str = "recorder";
/// `YYYY-MM-DDTHH:MM:SSZ`, the only timestamp form a header may use.
const TIMESTAMP_LEN: usize = 20;
const MAX_MONTH: u32 = 12;
const MAX_DAY: u32 = 31;
const MAX_HOUR: u32 = 23;
const MAX_MINUTE_SECOND: u32 = 59;

/// Whether a transcript was recorded from a real session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    /// Recorded from a real `claude` run; may whitelist response shapes.
    Live,
    /// Hand-written; never whitelists anything.
    Synthetic,
}

/// Why a transcript could not be loaded or trusted.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// The file could not be read.
    #[error("could not read fixture {0}: {1}")]
    Unreadable(String, String),
    /// The first line is not a recording header.
    #[error("the transcript has no recording header on its first line")]
    MissingHeader,
    /// The header label is neither `live` nor `synthetic`.
    #[error("the recording header label must be \"live\" or \"synthetic\"")]
    BadLabel,
    /// A live header lacks a field or holds an invalid one.
    #[error("the live recording header has a missing or invalid {0}")]
    IncompleteLiveHeader(&'static str),
    /// Only live transcripts may whitelist response shapes.
    #[error("only a live transcript may whitelist control_response shapes")]
    NotLive,
}

/// The structure of a `control_response` verdict: its behavior and field names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlShape {
    /// `allow` or `deny`.
    pub behavior: String,
    /// The verdict's field names, sorted.
    pub fields: BTreeSet<String>,
}

impl ControlShape {
    /// The shape of a `control_response` message, or `None` when it is not one.
    #[must_use]
    pub fn of(message: &Value) -> Option<Self> {
        if message["type"] != CONTROL_RESPONSE_TYPE {
            return None;
        }
        let verdict = message["response"]["response"].as_object()?;
        Some(Self {
            behavior: verdict.get("behavior")?.as_str()?.to_owned(),
            fields: verdict.keys().cloned().collect(),
        })
    }
}

/// A parsed transcript: a validated header and the lines after it.
#[derive(Debug, Clone)]
pub struct Transcript {
    label: Label,
    lines: Vec<String>,
}

impl Transcript {
    /// Parses transcript text, validating its header.
    ///
    /// # Errors
    /// Returns [`FixtureError`] when the header is missing, labelled
    /// unknown, or labelled live without a valid version, time and recorder.
    pub fn parse(text: &str) -> Result<Self, FixtureError> {
        let mut lines = text.lines();
        let header: Value = lines
            .next()
            .and_then(|line| serde_json::from_str(line).ok())
            .filter(|value: &Value| value["type"] == HEADER_TYPE)
            .ok_or(FixtureError::MissingHeader)?;
        let label = match header["label"].as_str() {
            Some(LABEL_LIVE) => {
                validate_live(&header)?;
                Label::Live
            }
            Some(LABEL_SYNTHETIC) => Label::Synthetic,
            _ => return Err(FixtureError::BadLabel),
        };
        Ok(Self {
            label,
            lines: lines
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }

    /// The label from the header.
    #[must_use]
    pub fn label(&self) -> Label {
        self.label
    }

    /// The lines Claude Code wrote, without the `control_response` lines coda sent.
    #[must_use]
    pub fn stream_lines(&self) -> Vec<&str> {
        self.lines
            .iter()
            .map(String::as_str)
            .filter(|line| !is_control_response(line))
            .collect()
    }

    /// The `control_response` shapes this transcript vouches for.
    ///
    /// # Errors
    /// Returns [`FixtureError::NotLive`] for a synthetic transcript.
    pub fn control_response_shapes(&self) -> Result<Vec<ControlShape>, FixtureError> {
        if self.label != Label::Live {
            return Err(FixtureError::NotLive);
        }
        Ok(self
            .lines
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|message| ControlShape::of(&message))
            .collect())
    }
}

fn is_control_response(line: &str) -> bool {
    serde_json::from_str::<Value>(line).is_ok_and(|v| v["type"] == CONTROL_RESPONSE_TYPE)
}

fn validate_live(header: &Value) -> Result<(), FixtureError> {
    let text = |field: &str| header[field].as_str().filter(|s| !s.trim().is_empty());
    for field in [FIELD_VERSION, FIELD_RECORDER] {
        if text(field).is_none() {
            return Err(FixtureError::IncompleteLiveHeader(field));
        }
    }
    if !text(FIELD_RECORDED_AT).is_some_and(is_timestamp) {
        return Err(FixtureError::IncompleteLiveHeader(FIELD_RECORDED_AT));
    }
    Ok(())
}

fn is_timestamp(stamp: &str) -> bool {
    let bytes = stamp.as_bytes();
    if bytes.len() != TIMESTAMP_LEN || !stamp.is_ascii() {
        return false;
    }
    let number = |range: std::ops::Range<usize>| stamp[range].parse::<u32>().ok();
    let separators = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'Z'),
    ];
    separators.iter().all(|&(at, sep)| bytes[at] == sep)
        && number(0..4).is_some()
        && number(5..7).is_some_and(|m| (1..=MAX_MONTH).contains(&m))
        && number(8..10).is_some_and(|d| (1..=MAX_DAY).contains(&d))
        && number(11..13).is_some_and(|h| h <= MAX_HOUR)
        && number(14..16).is_some_and(|m| m <= MAX_MINUTE_SECOND)
        && number(17..19).is_some_and(|s| s <= MAX_MINUTE_SECOND)
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(FIXTURE_DIR)
}

/// Loads one transcript from `fixtures/claude/` by file name.
///
/// # Errors
/// Returns [`FixtureError`] when the file is unreadable or its header is invalid.
pub fn load(name: &str) -> Result<Transcript, FixtureError> {
    let text = std::fs::read_to_string(fixture_dir().join(name))
        .map_err(|source| FixtureError::Unreadable(name.to_owned(), source.to_string()))?;
    Transcript::parse(&text)
}

/// Loads every committed transcript, sorted by file name.
///
/// # Errors
/// Returns [`FixtureError`] when the directory or any file is unreadable or invalid.
pub fn all() -> Result<Vec<(String, Transcript)>, FixtureError> {
    let unreadable = |source: std::io::Error| {
        FixtureError::Unreadable(FIXTURE_DIR.to_owned(), source.to_string())
    };
    let mut names = Vec::new();
    for entry in std::fs::read_dir(fixture_dir()).map_err(unreadable)? {
        let path = entry.map_err(unreadable)?.path();
        if path
            .extension()
            .is_some_and(|ext| ext == TRANSCRIPT_EXTENSION)
        {
            names.extend(path.file_name().and_then(|n| n.to_str()).map(str::to_owned));
        }
    }
    names.sort();
    names
        .into_iter()
        .map(|name| load(&name).map(|transcript| (name, transcript)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE_HEADER: &str = r#"{"type":"coda_recording","label":"live","claude_version":"2.1.292","recorded_at":"2026-05-01T10:00:00Z","recorder":"rec-1"}"#;
    const SYNTHETIC_HEADER: &str = r#"{"type":"coda_recording","label":"synthetic"}"#;
    const ALLOW: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"r1","response":{"behavior":"allow","updatedInput":{"command":"ls"}}}}"#;
    const DENY: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"r1","response":{"behavior":"deny","message":"no"}}}"#;
    const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"s"}"#;

    fn with_header(header: &str, rest: &[&str]) -> String {
        let mut text = String::from(header);
        for line in rest {
            text.push('\n');
            text.push_str(line);
        }
        text
    }

    #[test]
    fn a_live_transcript_with_a_full_header_parses() {
        // Arrange
        let text = with_header(LIVE_HEADER, &[INIT, ALLOW]);

        // Act
        let transcript = Transcript::parse(&text).unwrap();

        // Assert
        assert_eq!(transcript.label(), Label::Live);
        assert_eq!(transcript.stream_lines(), vec![INIT]);
    }

    #[test]
    fn a_transcript_without_a_header_is_rejected() {
        // Act
        let result = Transcript::parse(INIT);

        // Assert
        assert!(matches!(result, Err(FixtureError::MissingHeader)));
    }

    #[test]
    fn an_empty_transcript_is_rejected() {
        assert!(matches!(
            Transcript::parse(""),
            Err(FixtureError::MissingHeader)
        ));
    }

    #[test]
    fn an_unknown_label_is_rejected() {
        // Arrange
        let text = r#"{"type":"coda_recording","label":"recorded"}"#;

        // Act
        let result = Transcript::parse(text);

        // Assert
        assert!(matches!(result, Err(FixtureError::BadLabel)));
    }

    #[test]
    fn a_live_header_missing_a_field_is_rejected() {
        for field in ["claude_version", "recorded_at", "recorder"] {
            // Arrange
            let mut header: serde_json::Value = serde_json::from_str(LIVE_HEADER).unwrap();
            header.as_object_mut().unwrap().remove(field);

            // Act
            let result = Transcript::parse(&header.to_string());

            // Assert
            assert!(
                matches!(result, Err(FixtureError::IncompleteLiveHeader(f)) if f == field),
                "{field}"
            );
        }
    }

    #[test]
    fn a_live_header_with_a_malformed_timestamp_is_rejected() {
        for stamp in ["yesterday", "", "2026-05-01", "2026-13-01T10:00:00Z"] {
            // Arrange
            let mut header: serde_json::Value = serde_json::from_str(LIVE_HEADER).unwrap();
            header["recorded_at"] = stamp.into();

            // Act
            let result = Transcript::parse(&header.to_string());

            // Assert
            assert!(
                matches!(
                    result,
                    Err(FixtureError::IncompleteLiveHeader("recorded_at"))
                ),
                "{stamp:?}"
            );
        }
    }

    #[test]
    fn a_live_transcript_whitelists_its_control_response_shapes() {
        // Arrange
        let transcript = Transcript::parse(&with_header(LIVE_HEADER, &[ALLOW, DENY])).unwrap();

        // Act
        let shapes = transcript.control_response_shapes().unwrap();

        // Assert
        assert_eq!(shapes.len(), 2);
        assert_eq!(shapes[0].behavior, "allow");
        assert_eq!(
            shapes[0]
                .fields
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["behavior", "updatedInput"]
        );
        assert_eq!(shapes[1].behavior, "deny");
    }

    #[test]
    fn a_synthetic_transcript_cannot_whitelist_shapes() {
        // Arrange
        let transcript = Transcript::parse(&with_header(SYNTHETIC_HEADER, &[ALLOW])).unwrap();

        // Act
        let result = transcript.control_response_shapes();

        // Assert
        assert!(matches!(result, Err(FixtureError::NotLive)));
    }

    #[test]
    fn a_non_json_line_after_the_header_is_kept_as_a_stream_line() {
        // Arrange
        let text = with_header(SYNTHETIC_HEADER, &["{not json"]);

        // Act
        let transcript = Transcript::parse(&text).unwrap();

        // Assert
        assert_eq!(transcript.stream_lines(), vec!["{not json"]);
    }

    #[test]
    fn every_committed_fixture_has_a_valid_header_and_live_files_are_live() {
        // Act
        let all = all().unwrap();

        // Assert
        assert!(!all.is_empty());
        for (name, transcript) in all {
            if name.starts_with("live-") {
                assert_eq!(transcript.label(), Label::Live, "{name}");
            } else {
                assert_eq!(transcript.label(), Label::Synthetic, "{name}");
            }
        }
    }

    #[test]
    fn loading_a_missing_fixture_names_it() {
        // Act
        let result = load("absent.jsonl");

        // Assert
        assert!(matches!(result, Err(FixtureError::Unreadable(n, _)) if n == "absent.jsonl"));
    }
}
