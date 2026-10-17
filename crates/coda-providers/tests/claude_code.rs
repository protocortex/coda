// SPDX-License-Identifier: AGPL-3.0-only

//! Recorded and synthetic transcripts replayed through the stream parser.

use coda_protocol::types::PermissionDecision;
use coda_providers::claude_code::{Diagnostic, Message, StreamParser, control_response};
use coda_testkit::fixtures::{self, FixtureError, Label, Transcript};
use serde_json::json;

fn parse(transcript: &Transcript) -> (Vec<Message>, Vec<Diagnostic>) {
    let mut parser = StreamParser::new();
    let mut messages = Vec::new();
    for line in transcript.stream_lines() {
        messages.extend(parser.feed(format!("{line}\n").as_bytes()));
    }
    (messages, parser.diagnostics().to_vec())
}

fn kinds(messages: &[Message]) -> Vec<&'static str> {
    messages
        .iter()
        .map(|m| match m {
            Message::Init { .. } => "init",
            Message::TextDelta(_) => "delta",
            Message::Assistant { .. } => "assistant",
            Message::Result { .. } => "result",
            Message::PermissionRequest(_) => "ask",
            Message::InvalidPermissionRequest { .. } => "invalid",
        })
        .collect()
}

#[test]
fn the_synthetic_allow_transcript_replays_a_completed_turn() {
    // Arrange
    let transcript = fixtures::load("synthetic-allow.jsonl").unwrap();

    // Act
    let (messages, diagnostics) = parse(&transcript);

    // Assert
    assert_eq!(
        kinds(&messages),
        ["init", "delta", "ask", "assistant", "result"]
    );
    assert_eq!(diagnostics, vec![]);
}

#[test]
fn the_synthetic_deny_transcript_replays_the_denial_text() {
    // Arrange
    let transcript = fixtures::load("synthetic-deny.jsonl").unwrap();

    // Act
    let (messages, _) = parse(&transcript);

    // Assert
    assert!(messages.contains(&Message::Assistant {
        text: "denied".into()
    }));
}

#[test]
fn the_synthetic_crash_transcript_ends_before_a_result() {
    // Arrange
    let transcript = fixtures::load("synthetic-crash.jsonl").unwrap();

    // Act
    let (messages, _) = parse(&transcript);

    // Assert
    assert_eq!(kinds(&messages), ["init", "delta"]);
}

#[test]
fn the_synthetic_malformed_transcript_records_diagnostics_and_continues() {
    // Arrange
    let transcript = fixtures::load("synthetic-malformed.jsonl").unwrap();

    // Act
    let (messages, diagnostics) = parse(&transcript);

    // Assert
    assert_eq!(kinds(&messages), ["init", "delta", "result"]);
    assert!(matches!(diagnostics[0], Diagnostic::MalformedLine { .. }));
    assert!(matches!(diagnostics[1], Diagnostic::UnknownType { .. }));
}

#[test]
fn synthetic_transcripts_cannot_whitelist_control_response_shapes() {
    for name in ["synthetic-allow.jsonl", "synthetic-deny.jsonl"] {
        // Arrange
        let transcript = fixtures::load(name).unwrap();

        // Act
        let result = transcript.control_response_shapes();

        // Assert
        assert_eq!(transcript.label(), Label::Synthetic);
        assert!(matches!(result, Err(FixtureError::NotLive)), "{name}");
    }
}

/// Outstanding until live transcripts are recorded from a real session:
/// `fixtures/claude/live-allow.jsonl` and `live-deny.jsonl` must carry a live
/// header, and the adapter's responses must match their recorded shapes.
#[test]
#[ignore = "waiting for recorded live transcripts live-allow.jsonl and live-deny.jsonl"]
fn the_adapter_responses_match_the_recorded_live_shapes() {
    for (name, decision) in [
        ("live-allow.jsonl", PermissionDecision::AllowOnce),
        ("live-deny.jsonl", PermissionDecision::Deny),
    ] {
        // Arrange
        let transcript = fixtures::load(name).unwrap();
        let recorded = transcript.control_response_shapes().unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&control_response("r", decision, &json!({"command": "ls"})))
                .unwrap();

        // Act
        let shape = fixtures::ControlShape::of(&written).unwrap();

        // Assert
        assert_eq!(transcript.label(), Label::Live, "{name}");
        assert!(recorded.contains(&shape), "{name}");
    }
}
