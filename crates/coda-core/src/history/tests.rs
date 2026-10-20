// SPDX-License-Identifier: AGPL-3.0-only

use std::io::Write;
use std::sync::{Arc, Mutex};

use coda_protocol::events::STREAM_DELTA_KIND;
use coda_protocol::types::{OriginalId, TurnId};
use coda_store::StoredEvent;
use serde_json::{Value, json};

use super::*;
use crate::prompt::{Message, Prefix, ToolCall};

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn event(seq: u64, kind: &str, body: &Value) -> StoredEvent {
    StoredEvent {
        seq,
        turn: TurnId::new("1"),
        kind: kind.to_owned(),
        body: body.to_string(),
        at: 0,
    }
}

fn numbered(events: &[(&str, Value)]) -> Vec<StoredEvent> {
    (1..)
        .zip(events)
        .map(|(seq, (kind, body))| event(seq, kind, body))
        .collect()
}

const GONE: &str = "gone";

fn prefix() -> Prefix {
    Prefix::new("You are Coda.", Vec::new(), "").unwrap()
}

fn user(text: &str) -> (&'static str, Value) {
    (USER_MESSAGE_KIND, json!({ "text": text }))
}

/// A reply whose call arguments are kept under an id that is the text itself.
fn assistant(text: &str, calls: &[ToolCall]) -> (&'static str, Value) {
    let kept: Vec<_> = calls
        .iter()
        .map(|call| (call, Some(OriginalId::new(call.arguments.clone()))))
        .collect();
    (ASSISTANT_MESSAGE_KIND, assistant_body(text, &kept))
}

/// A finished call whose result is kept under an id that is the text itself,
/// so the lookup below needs no fake store.
fn finished(call_id: &str, text: &str, is_error: bool) -> (&'static str, Value) {
    let original = OriginalId::new(text);
    (
        TOOL_FINISHED_KIND,
        tool_finished_body("read", call_id, Some(&original), is_error),
    )
}

fn lookup(id: &OriginalId) -> Option<String> {
    (id.as_str() != GONE).then(|| id.as_str().to_owned())
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: "read".to_owned(),
        arguments: json!({ "path": "a.txt" }).to_string(),
    }
}

fn replay(events: &[(&str, Value)]) -> Vec<Message> {
    rebuild(prefix(), &numbered(events), &lookup)
        .messages()
        .to_vec()
}

/// Replays `events` and returns what the replay logged next to the messages.
fn replay_logged(events: &[(&str, Value)]) -> (Vec<Message>, String) {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer({
            let logs = logs.clone();
            move || logs.clone()
        })
        .finish();
    let messages = tracing::subscriber::with_default(subscriber, || replay(events));
    (messages, logs.text())
}

fn text_reply(text: &str) -> Message {
    Message::Assistant {
        text: text.to_owned(),
        tool_calls: Vec::new(),
    }
}

#[test]
fn a_stored_tool_session_replays_into_user_assistant_and_result_messages() {
    // Arrange
    let events = [
        user("go"),
        ("stream_delta", json!({"text": "ignored"})),
        assistant("", &[call("c1")]),
        finished("c1", "file text", false),
        ("usage", json!({})),
        assistant("done", &[]),
        ("turn_stopped", json!({"condition": "end_turn"})),
        ("result", json!({})),
    ];

    // Act
    let messages = replay(&events);

    // Assert
    assert_eq!(
        messages,
        [
            Message::User("go".to_owned()),
            Message::Assistant {
                text: String::new(),
                tool_calls: vec![call("c1")],
            },
            Message::ToolResult {
                call_id: "c1".to_owned(),
                text: "file text".to_owned(),
                is_error: false,
            },
            text_reply("done"),
        ]
    );
}

#[test]
fn an_unknown_event_kind_is_skipped_with_a_warning() {
    // Arrange
    let events = [
        user("hi"),
        ("from_the_future", json!({})),
        assistant("hello", &[]),
    ];

    // Act
    let (messages, logs) = replay_logged(&events);

    // Assert
    assert_eq!(
        messages,
        [Message::User("hi".to_owned()), text_reply("hello")]
    );
    assert!(logs.contains("from_the_future"), "{logs}");
}

#[test]
fn a_legacy_conversation_reset_is_skipped_silently_and_earlier_history_stays() {
    // Arrange
    let events = [
        user("before"),
        assistant("old answer", &[]),
        (CONVERSATION_RESET_KIND, json!({})),
        user("after"),
        assistant("new answer", &[]),
    ];

    // Act
    let (messages, logs) = replay_logged(&events);

    // Assert
    assert_eq!(
        messages,
        [
            Message::User("before".to_owned()),
            text_reply("old answer"),
            Message::User("after".to_owned()),
            text_reply("new answer"),
        ]
    );
    assert_eq!(logs, "", "the legacy kind is not worth a warning");
}

#[test]
fn a_send_that_ended_before_any_reply_replays_with_the_no_reply_marker() {
    // Arrange
    let events = [user("cancelled"), user("next"), assistant("ok", &[])];

    // Act
    let messages = replay(&events);

    // Assert
    assert_eq!(
        messages,
        [
            Message::User("cancelled".to_owned()),
            text_reply(NO_REPLY),
            Message::User("next".to_owned()),
            text_reply("ok"),
        ]
    );
}

#[test]
fn a_turn_cut_off_between_tool_calls_answers_the_missing_ones_as_skipped() {
    // Arrange
    let events = [
        user("go"),
        assistant("", &[call("c1"), call("c2")]),
        finished("c1", "one", false),
        user("again"),
    ];

    // Act
    let messages = replay(&events);

    // Assert
    assert_eq!(
        messages[1..],
        [
            Message::Assistant {
                text: String::new(),
                tool_calls: vec![call("c1"), call("c2")],
            },
            Message::ToolResult {
                call_id: "c1".to_owned(),
                text: "one".to_owned(),
                is_error: false,
            },
            Message::ToolResult {
                call_id: "c2".to_owned(),
                text: SKIPPED.to_owned(),
                is_error: true,
            },
            text_reply(NO_REPLY),
            Message::User("again".to_owned()),
            text_reply(NO_REPLY),
        ]
    );
}

#[test]
fn a_session_that_ends_on_a_user_message_is_left_ready_for_the_next_send() {
    // Arrange
    let events = [user("pending")];

    // Act
    let messages = replay(&events);

    // Assert
    assert_eq!(
        messages,
        [Message::User("pending".to_owned()), text_reply(NO_REPLY)]
    );
}

#[test]
fn a_tool_result_for_a_call_nobody_made_is_skipped_with_a_warning() {
    // Arrange
    let events = [
        user("go"),
        assistant("hi", &[]),
        finished("ghost", "x", false),
    ];

    // Act
    let (messages, logs) = replay_logged(&events);

    // Assert
    assert_eq!(messages, [Message::User("go".to_owned()), text_reply("hi")]);
    assert!(logs.contains("ghost"), "{logs}");
}

#[test]
fn a_tool_result_that_can_no_longer_be_read_is_replaced_by_a_note() {
    // Arrange
    let events = [
        user("go"),
        assistant("", &[call("c1")]),
        finished("c1", GONE, false),
        assistant("done", &[]),
    ];

    // Act
    let messages = replay(&events);

    // Assert
    assert_eq!(
        messages[2],
        Message::ToolResult {
            call_id: "c1".to_owned(),
            text: RESULT_UNAVAILABLE.to_owned(),
            is_error: false,
        }
    );
}

#[test]
fn a_tool_finished_body_from_an_older_daemon_is_skipped_without_a_warning() {
    // Arrange
    let legacy = ("tool_finished", json!({"tool": "read", "is_error": false}));
    let events = [user("go"), assistant("hi", &[]), legacy];

    // Act
    let (messages, logs) = replay_logged(&events);

    // Assert
    assert_eq!(messages, [Message::User("go".to_owned()), text_reply("hi")]);
    assert_eq!(logs, "");
}

#[test]
fn call_arguments_that_can_no_longer_be_read_replay_as_an_empty_object() {
    // Arrange
    let lost = ToolCall {
        arguments: GONE.to_owned(),
        ..call("c1")
    };
    let events = [
        user("go"),
        assistant("", &[lost]),
        finished("c1", "x", false),
    ];

    // Act
    let messages = replay(&events);

    // Assert
    let Message::Assistant { tool_calls, .. } = &messages[1] else {
        panic!("expected an assistant message: {messages:?}");
    };
    assert_eq!(tool_calls[0].arguments, "{}");
}

#[test]
fn empty_arguments_and_results_replay_as_empty_text() {
    // Arrange
    let silent = ToolCall {
        arguments: String::new(),
        ..call("c1")
    };
    let events = [
        user("go"),
        assistant("", &[silent]),
        finished("c1", "", false),
    ];

    // Act
    let messages = replay(&events);

    // Assert
    assert_eq!(
        messages[1..],
        [
            Message::Assistant {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    arguments: String::new(),
                    ..call("c1")
                }],
            },
            Message::ToolResult {
                call_id: "c1".to_owned(),
                text: String::new(),
                is_error: false,
            },
            text_reply(NO_REPLY),
        ]
    );
}

#[test]
fn bookkeeping_events_are_not_worth_a_warning() {
    // Arrange
    let events = [
        user("go"),
        (STREAM_DELTA_KIND, json!({"text": "x"})),
        (PERMISSION_REQUEST_KIND, json!({})),
        (MODEL_SWITCHED_KIND, json!({})),
        assistant("hi", &[]),
    ];

    // Act
    let (_, logs) = replay_logged(&events);

    // Assert
    assert_eq!(logs, "");
}
