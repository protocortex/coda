// SPDX-License-Identifier: AGPL-3.0-only

//! The Anthropic wire layer: SSE framing, event mapping, request building,
//! status classification and retry timing. All fixtures are synthetic.

use std::time::{Duration, SystemTime};

use coda_providers::anthropic::events::EventMapper;
use coda_providers::anthropic::request::{RequestSettings, ToolSchema, build_body};
use coda_providers::anthropic::sse::{MAX_EVENT_BYTES, SseError, SseParser};
use coda_providers::anthropic::{Timing, classify_status};
use coda_providers::model::{
    ModelEvent, ModelRequest, ProviderError, RequestMessage, RequestToolCall, StopReason, Usage,
};
use serde_json::json;

fn frame(data: &serde_json::Value) -> String {
    format!("event: x\ndata: {data}\n\n")
}

fn message_start(input: u64, cache_write: u64, cache_read: u64) -> String {
    frame(&json!({"type": "message_start", "message": {"usage": {
        "input_tokens": input,
        "cache_creation_input_tokens": cache_write,
        "cache_read_input_tokens": cache_read,
        "output_tokens": 1,
    }}}))
}

fn block_start(index: u32, block: &serde_json::Value) -> String {
    frame(&json!({"type": "content_block_start", "index": index, "content_block": block}))
}

fn delta(index: u32, delta: &serde_json::Value) -> String {
    frame(&json!({"type": "content_block_delta", "index": index, "delta": delta}))
}

fn message_end(reason: &str, output: u64) -> String {
    let message_delta = frame(&json!({"type": "message_delta",
        "delta": {"stop_reason": reason}, "usage": {"output_tokens": output}}));
    message_delta + &frame(&json!({"type": "message_stop"}))
}

/// Feeds a whole stream through parser and mapper in one chunk.
fn run(stream: &str) -> (Vec<ModelEvent>, Result<bool, ProviderError>, bool) {
    let mut parser = SseParser::default();
    let mut mapper = EventMapper::default();
    let mut events = Vec::new();
    let mut done = false;
    for data in parser.push(stream.as_bytes()).expect("well-formed frames") {
        match mapper.handle(&data) {
            Ok(step) => {
                events.extend(step.events);
                done = step.done;
            }
            Err(error) => return (events, Err(error), mapper.output_started()),
        }
    }
    (events, Ok(done), mapper.output_started())
}

fn request() -> ModelRequest {
    ModelRequest::user_prompt("hello")
}

fn settings(system: &str, tools: Vec<ToolSchema>) -> RequestSettings {
    RequestSettings {
        model: "test-model".to_owned(),
        max_tokens: 64,
        system: system.to_owned(),
        tools,
    }
}

fn tool(name: &str) -> ToolSchema {
    ToolSchema {
        name: name.to_owned(),
        description: "d".to_owned(),
        input_schema: json!({"type": "object"}),
    }
}

#[test]
fn sse_event_split_across_chunks_is_emitted_once_complete() {
    // Arrange
    let mut parser = SseParser::default();

    // Act
    let first = parser.push(b"event: a\ndata: {\"x\"").expect("push");
    let second = parser.push(b":1}\n").expect("push");
    let third = parser.push(b"\n").expect("push");

    // Assert
    assert_eq!(first, Vec::<String>::new());
    assert_eq!(second, Vec::<String>::new());
    assert_eq!(third, vec!["{\"x\":1}".to_owned()]);
}

#[test]
fn sse_event_split_inside_a_multibyte_character_is_decoded_whole() {
    // Arrange
    let mut parser = SseParser::default();
    let bytes = "data: caf\u{e9}\n\n".as_bytes();
    let split = bytes.iter().position(|b| *b == 0xC3).expect("lead byte") + 1;

    // Act
    let first = parser.push(&bytes[..split]).expect("push");
    let second = parser.push(&bytes[split..]).expect("push");

    // Assert
    assert_eq!(first, Vec::<String>::new());
    assert_eq!(second, vec!["caf\u{e9}".to_owned()]);
}

#[test]
fn sse_handles_crlf_comments_and_multiline_data() {
    // Arrange
    let mut parser = SseParser::default();

    // Act
    let events = parser
        .push(b": keep-alive\r\nevent: a\r\ndata: one\r\ndata: two\r\n\r\ndata:three\n\n")
        .expect("push");

    // Assert
    assert_eq!(events, vec!["one\ntwo".to_owned(), "three".to_owned()]);
}

#[test]
fn sse_unterminated_trailing_event_is_not_emitted() {
    // Arrange
    let mut parser = SseParser::default();

    // Act
    let events = parser.push(b"data: a\n\ndata: b\n").expect("push");

    // Assert
    assert_eq!(events, vec!["a".to_owned()]);
}

#[test]
fn sse_event_larger_than_the_limit_is_rejected() {
    // Arrange
    let mut parser = SseParser::default();
    let oversized = vec![b'a'; MAX_EVENT_BYTES + 1];

    // Act
    let result = parser.push(&oversized);

    // Assert
    assert_eq!(result, Err(SseError::EventTooLarge));
}

#[test]
fn sse_invalid_utf8_in_a_complete_event_is_rejected() {
    // Arrange
    let mut parser = SseParser::default();

    // Act
    let result = parser.push(b"data: \xff\n\n");

    // Assert
    assert_eq!(result, Err(SseError::InvalidUtf8));
}

#[test]
fn text_stream_maps_to_deltas_usage_and_stop() {
    // Arrange
    let stream = message_start(10, 5, 2)
        + &block_start(0, &json!({"type": "text", "text": ""}))
        + &delta(0, &json!({"type": "text_delta", "text": "Hel"}))
        + &delta(0, &json!({"type": "text_delta", "text": "lo"}))
        + &frame(&json!({"type": "content_block_stop", "index": 0}))
        + &frame(&json!({"type": "ping"}))
        + &message_end("end_turn", 7);

    // Act
    let (events, result, started) = run(&stream);

    // Assert
    assert_eq!(result, Ok(true));
    assert!(started);
    assert_eq!(
        events,
        vec![
            ModelEvent::TextDelta {
                text: "Hel".to_owned()
            },
            ModelEvent::TextDelta {
                text: "lo".to_owned()
            },
            ModelEvent::Usage(Usage {
                input_tokens: 17,
                output_tokens: 7
            }),
            ModelEvent::Stop {
                reason: StopReason::EndTurn
            },
        ]
    );
}

#[test]
fn tool_call_arguments_split_across_deltas_keep_their_call_id() {
    // Arrange
    let stream = message_start(1, 0, 0)
        + &block_start(
            1,
            &json!({"type": "tool_use", "id": "toolu_1", "name": "read", "input": {}}),
        )
        + &delta(
            1,
            &json!({"type": "input_json_delta", "partial_json": "{\"pa"}),
        )
        + &delta(
            1,
            &json!({"type": "input_json_delta", "partial_json": "th\":\"a\"}"}),
        )
        + &frame(&json!({"type": "content_block_stop", "index": 1}))
        + &message_end("tool_use", 3);

    // Act
    let (events, result, _) = run(&stream);

    // Assert
    assert_eq!(result, Ok(true));
    assert_eq!(
        events[..3],
        [
            ModelEvent::ToolCallStart {
                call_id: "toolu_1".to_owned(),
                name: "read".to_owned()
            },
            ModelEvent::ToolCallArgumentsDelta {
                call_id: "toolu_1".to_owned(),
                fragment: "{\"pa".to_owned()
            },
            ModelEvent::ToolCallArgumentsDelta {
                call_id: "toolu_1".to_owned(),
                fragment: "th\":\"a\"}".to_owned()
            },
        ]
    );
    assert_eq!(
        events[4],
        ModelEvent::Stop {
            reason: StopReason::ToolUse
        }
    );
}

#[test]
fn tool_call_without_arguments_gets_an_empty_object() {
    // Arrange
    let stream = message_start(1, 0, 0)
        + &block_start(
            0,
            &json!({"type": "tool_use", "id": "toolu_2", "name": "ls", "input": {}}),
        )
        + &frame(&json!({"type": "content_block_stop", "index": 0}))
        + &message_end("tool_use", 1);

    // Act
    let (events, _, _) = run(&stream);

    // Assert
    assert_eq!(
        events[1],
        ModelEvent::ToolCallArgumentsDelta {
            call_id: "toolu_2".to_owned(),
            fragment: "{}".to_owned()
        }
    );
}

#[test]
fn max_tokens_stop_reason_is_reported() {
    // Arrange
    let stream = message_start(1, 0, 0) + &message_end("max_tokens", 9);

    // Act
    let (events, result, _) = run(&stream);

    // Assert
    assert_eq!(result, Ok(true));
    assert_eq!(
        events.last(),
        Some(&ModelEvent::Stop {
            reason: StopReason::MaxTokens
        })
    );
}

#[test]
fn unknown_stop_reason_is_fatal() {
    // Arrange
    let stream = message_start(1, 0, 0) + &message_end("something_new", 9);

    // Act
    let (_, result, _) = run(&stream);

    // Assert
    assert!(
        matches!(result, Err(ProviderError::Fatal { .. })),
        "{result:?}"
    );
}

#[test]
fn empty_stream_yields_no_events_and_no_output() {
    // Arrange / Act
    let (events, result, started) = run("");

    // Assert
    assert_eq!(events, Vec::<ModelEvent>::new());
    assert_eq!(result, Ok(false));
    assert!(!started);
}

#[test]
fn in_stream_errors_are_classified_and_output_state_is_reported() {
    // Arrange
    let overloaded = frame(&json!({"type": "error",
        "error": {"type": "overloaded_error", "message": "busy"}}));
    let invalid = frame(&json!({"type": "error",
        "error": {"type": "invalid_request_error", "message": "bad"}}));
    let overflow = frame(&json!({"type": "error",
        "error": {"type": "invalid_request_error", "message": "prompt is too long: 9 > 8"}}));

    // Act
    let (_, early, early_started) = run(&overloaded);
    let (_, late, late_started) = run(&(message_start(1, 0, 0) + &overloaded));
    let (_, fatal, _) = run(&invalid);
    let (_, exceeded, _) = run(&overflow);

    // Assert
    assert!(matches!(early, Err(ProviderError::Retryable { .. })));
    assert!(!early_started);
    assert!(matches!(late, Err(ProviderError::Retryable { .. })));
    assert!(late_started);
    assert!(matches!(fatal, Err(ProviderError::Fatal { .. })));
    assert_eq!(exceeded, Err(ProviderError::ContextWindowExceeded));
}

#[test]
fn request_body_marks_the_system_block_and_last_tool_for_caching() {
    // Arrange
    let settings = settings("be brief", vec![tool("read"), tool("write")]);

    // Act
    let body = build_body(&settings, &request());

    // Assert
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["max_tokens"], 64);
    assert_eq!(body["stream"], true);
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "be brief", "cache_control": {"type": "ephemeral"}}])
    );
    assert_eq!(body["tools"][0].get("cache_control"), None);
    assert_eq!(
        body["tools"][1]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(body["tools"][1]["input_schema"], json!({"type": "object"}));
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": "hello"}])
    );
}

#[test]
fn request_body_omits_empty_system_and_tools() {
    // Arrange
    let settings = settings("", Vec::new());

    // Act
    let body = build_body(&settings, &request());

    // Assert
    assert_eq!(body.get("system"), None);
    assert_eq!(body.get("tools"), None);
}

#[test]
fn statuses_are_classified_by_retryability() {
    // Arrange
    let retryable = [429, 500, 502, 503, 504, 529];
    let fatal = [400, 401, 403, 404, 408, 409];

    for status in retryable {
        // Act
        let error = classify_status(status, "");

        // Assert
        assert!(matches!(error, ProviderError::Retryable { .. }), "{status}");
    }
    for status in fatal {
        // Act
        let error = classify_status(status, "");

        // Assert
        assert!(matches!(error, ProviderError::Fatal { .. }), "{status}");
    }
}

#[test]
fn a_400_that_reports_context_overflow_is_typed() {
    // Arrange
    let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#;

    // Act
    let error = classify_status(400, body);

    // Assert
    assert_eq!(error, ProviderError::ContextWindowExceeded);
}

#[test]
fn error_message_carries_the_api_message_but_stays_short() {
    // Arrange
    let long = "x".repeat(1000);
    let body = json!({"error": {"type": "invalid_request_error", "message": long}}).to_string();

    // Act
    let ProviderError::Fatal { message } = classify_status(400, &body) else {
        panic!("expected fatal");
    };

    // Assert
    assert!(message.contains("400"));
    assert!(message.len() < 400, "{}", message.len());
}

#[test]
fn backoff_doubles_from_the_base_and_the_header_wins_when_present() {
    // Arrange
    let timing = Timing::default();
    let now = SystemTime::UNIX_EPOCH;

    // Act / Assert
    assert_eq!(timing.max_attempts, 3);
    assert_eq!(timing.retry_delay(1, None, now), Duration::from_millis(500));
    assert_eq!(
        timing.retry_delay(2, None, now),
        Duration::from_millis(1000)
    );
    assert_eq!(
        timing.retry_delay(1, Some("2"), now),
        Duration::from_secs(2)
    );
    assert_eq!(
        timing.retry_delay(1, Some("junk"), now),
        Duration::from_millis(500)
    );
    assert_eq!(
        timing.retry_delay(1, Some("100000"), now),
        timing.backoff_max
    );
}

#[test]
fn an_http_date_retry_after_waits_for_the_time_remaining() {
    // Arrange
    let timing = Timing::default();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_778);
    let date = "Sun, 06 Nov 1994 08:49:40 GMT";

    // Act
    let delay = timing.retry_delay(1, Some(date), now);

    // Assert
    assert_eq!(delay, Duration::from_secs(2));
}

#[test]
fn an_http_date_in_the_past_falls_back_to_no_wait_and_far_ones_are_clamped() {
    // Arrange
    let timing = Timing::default();
    let date = "Sun, 06 Nov 1994 08:49:37 GMT";
    let past = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_801);
    let long_before = SystemTime::UNIX_EPOCH;

    // Act
    let elapsed = timing.retry_delay(1, Some(date), past);
    let far = timing.retry_delay(1, Some(date), long_before);

    // Assert
    assert_eq!(elapsed, Duration::ZERO);
    assert_eq!(far, timing.backoff_max);
}

#[test]
fn request_too_large_and_context_limit_messages_are_context_overflow() {
    // Arrange
    let limit = r#"{"error":{"type":"invalid_request_error","message":"Input length and `max_tokens` exceed context limit: 188240 + 20000 > 200000"}}"#;
    let too_large =
        r#"{"error":{"type":"request_too_large","message":"Request exceeds the maximum size"}}"#;

    // Act
    let from_limit = classify_status(400, limit);
    let from_413 = classify_status(413, too_large);
    let bare_413 = classify_status(413, "");

    // Assert
    assert_eq!(from_limit, ProviderError::ContextWindowExceeded);
    assert_eq!(from_413, ProviderError::ContextWindowExceeded);
    assert_eq!(bare_413, ProviderError::ContextWindowExceeded);
}

#[test]
fn refusal_and_pause_turn_are_distinct_terminal_stop_reasons() {
    // Arrange
    let refusal = message_start(1, 0, 0) + &message_end("refusal", 2);
    let paused = message_start(1, 0, 0) + &message_end("pause_turn", 2);

    // Act
    let (refused, refusal_result, _) = run(&refusal);
    let (pause, pause_result, _) = run(&paused);

    // Assert
    assert_eq!(refusal_result, Ok(true));
    assert_eq!(pause_result, Ok(true));
    assert_eq!(
        refused.last(),
        Some(&ModelEvent::Stop {
            reason: StopReason::Refusal
        })
    );
    assert_eq!(
        pause.last(),
        Some(&ModelEvent::Stop {
            reason: StopReason::Paused
        })
    );
}

#[test]
fn request_body_is_byte_stable_whatever_the_tool_order() {
    // Arrange
    let forward = settings("s", vec![tool("write"), tool("bash"), tool("read")]);
    let backward = settings("s", vec![tool("read"), tool("bash"), tool("write")]);

    // Act
    let first = build_body(&forward, &request());
    let second = build_body(&backward, &request());

    // Assert
    assert_eq!(first.to_string(), second.to_string());
    let names: Vec<_> = first["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["bash", "read", "write"]);
    assert_eq!(first["tools"][0].get("cache_control"), None);
    assert_eq!(
        first["tools"][2]["cache_control"],
        json!({"type": "ephemeral"})
    );
}

#[test]
fn sse_finish_flushes_an_unterminated_final_event() {
    // Arrange
    let mut parser = SseParser::default();
    let pushed = parser.push(b"data: a\n\ndata: b").expect("push");

    // Act
    let flushed = parser.finish().expect("finish");

    // Assert
    assert_eq!(pushed, vec!["a".to_owned()]);
    assert_eq!(flushed, vec!["b".to_owned()]);
}

#[test]
fn sse_strips_a_leading_bom_even_when_split_across_chunks() {
    // Arrange
    let mut parser = SseParser::default();

    // Act
    let first = parser.push(b"\xEF\xBB").expect("push");
    let second = parser.push(b"\xBFdata: a\n\n").expect("push");

    // Assert
    assert_eq!(first, Vec::<String>::new());
    assert_eq!(second, vec!["a".to_owned()]);
}

#[test]
fn sse_accepts_lone_cr_line_endings() {
    // Arrange
    let mut parser = SseParser::default();

    // Act
    let events = parser.push(b"data: a\r\rdata: b\r\r").expect("push");
    let tail = parser.push(b"data: c\r").expect("push");
    let flushed = parser.finish().expect("finish");

    // Assert
    assert_eq!(events, vec!["a".to_owned()]);
    assert_eq!(tail, vec!["b".to_owned()]);
    assert_eq!(flushed, vec!["c".to_owned()]);
}

#[test]
fn sse_many_small_data_lines_still_hit_the_event_limit() {
    // Arrange
    let mut parser = SseParser::default();
    let line = format!(
        "data: {}
",
        "a".repeat(1023)
    );
    let mut result = Ok(Vec::new());

    // Act
    for _ in 0..=(MAX_EVENT_BYTES / 1023) {
        result = parser.push(line.as_bytes());
        if result.is_err() {
            break;
        }
    }

    // Assert
    assert_eq!(result, Err(SseError::EventTooLarge));
}

fn call(id: &str, name: &str, arguments: &str) -> RequestToolCall {
    RequestToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

fn tool_result(call_id: &str, text: &str, is_error: bool) -> RequestMessage {
    RequestMessage::ToolResult {
        call_id: call_id.to_owned(),
        text: text.to_owned(),
        is_error,
    }
}

fn tool_history() -> Vec<RequestMessage> {
    vec![
        RequestMessage::User("list files".to_owned()),
        RequestMessage::Assistant {
            text: "looking".to_owned(),
            tool_calls: vec![
                call("t1", "ls", r#"{"path":"."}"#),
                call("t2", "cat", r#"{"path":"a"}"#),
            ],
        },
        tool_result("t1", "a b", false),
        tool_result("t2", "denied", true),
    ]
}

#[test]
fn request_body_encodes_tool_history_with_results_in_one_user_message() {
    // Arrange
    let request = ModelRequest {
        messages: tool_history(),
        ..ModelRequest::default()
    };

    // Act
    let body = build_body(&settings("", Vec::new()), &request);

    // Assert
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "list files"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "looking"},
                {"type": "tool_use", "id": "t1", "name": "ls", "input": {"path": "."}},
                {"type": "tool_use", "id": "t2", "name": "cat", "input": {"path": "a"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "a b"},
                {"type": "tool_result", "tool_use_id": "t2", "content": "denied", "is_error": true},
            ]},
        ])
    );
}

#[test]
fn request_body_omits_empty_assistant_text_and_survives_invalid_arguments() {
    // Arrange
    let request = ModelRequest {
        messages: vec![RequestMessage::Assistant {
            text: String::new(),
            tool_calls: vec![call("t1", "ls", "{not json")],
        }],
        ..ModelRequest::default()
    };

    // Act
    let body = build_body(&settings("", Vec::new()), &request);

    // Assert
    assert_eq!(
        body["messages"],
        json!([{"role": "assistant", "content": [
            {"type": "tool_use", "id": "t1", "name": "ls", "input": {}},
        ]}])
    );
}

#[test]
fn request_prefix_overrides_provider_settings_and_sorts_tools() {
    // Arrange
    let request = ModelRequest {
        system: "from request".to_owned(),
        tools: vec![tool("write"), tool("read")],
        ..ModelRequest::user_prompt("hi")
    };

    // Act
    let body = build_body(&settings("from settings", vec![tool("other")]), &request);

    // Assert
    assert_eq!(body["system"][0]["text"], "from request");
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(body["tools"][1]["name"], "write");
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
}

#[test]
fn appending_a_message_leaves_earlier_message_bytes_unchanged() {
    // Arrange
    let history = tool_history();
    let shorter = ModelRequest {
        system: "s".to_owned(),
        tools: vec![tool("ls")],
        messages: history[..3].to_vec(),
    };
    let mut longer = shorter.clone();
    longer.messages.push(history[3].clone());
    longer
        .messages
        .push(RequestMessage::User("next".to_owned()));

    // Act
    let before = build_body(&settings("", Vec::new()), &shorter);
    let after = build_body(&settings("", Vec::new()), &longer);

    // Assert
    assert_eq!(before["system"], after["system"]);
    assert_eq!(before["tools"], after["tools"]);
    assert_eq!(before["messages"][0], after["messages"][0]);
    assert_eq!(before["messages"][1]["role"], "assistant");
    assert_eq!(before["messages"][1], after["messages"][1]);
}
