// SPDX-License-Identifier: Apache-2.0

//! Claude Code stream-json parsing and tool-name mapping.

use std::path::{Path, PathBuf};

use coda_protocol::events::{RateLimit, RateLimitWindow};
use coda_protocol::types::{ContextFill, PermissionDecision, TokenUsage};
use serde_json::{Value, json};

use crate::jsonl::{DiagnosticLog, Framed, LineFramer, bounded};
use crate::tool_call::{CallInfo, claude_call};
use crate::tool_request::{Tool, ToolRequest};

const CAN_USE_TOOL: &str = "can_use_tool";
const DENY_MESSAGE: &str = "denied by coda";
/// Claude Code reports a window's utilization as a fraction of one.
const BASIS_POINTS_PER_UNIT: f64 = 10_000.0;
/// Reset times are seconds. A larger number is milliseconds or garbage; this is year 5138 in seconds.
const MAX_EPOCH_SECONDS: u64 = 100_000_000_000;
const WINDOW_WITHOUT_UTILIZATION: &str = "rate limit window without a numeric utilization";
const UNUSABLE_RESET_TIME: &str = "rate limit reset time is not whole seconds since the epoch";

/// One message from the Claude Code output stream that coda acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// The session started; carries the id used with `--resume`.
    Init {
        /// Claude Code's session id.
        session_id: String,
        /// Where the API key came from, `none` when no key backs the session.
        api_key_source: Option<String>,
    },
    /// A streamed fragment of model text.
    TextDelta(String),
    /// A complete assistant message, text blocks joined.
    Assistant {
        /// The message text.
        text: String,
    },
    /// The model began a tool call; Claude Code runs it, perhaps with no permission ask.
    ToolStarted {
        /// Claude Code's id for the call.
        id: String,
        /// The tool's name, such as `Bash`.
        name: String,
        /// What the call works on, when its input says.
        summary: Option<String>,
    },
    /// A tool call finished and its result went back to the model.
    ToolFinished {
        /// The id the call started with.
        id: String,
        /// Whether the result was marked as an error.
        failed: bool,
    },
    /// The turn finished.
    Result {
        /// Whether the turn ended in an error.
        is_error: bool,
    },
    /// The tokens the finished turn used; arrives just before its [`Message::Result`].
    Usage(TokenUsage),
    /// How full the context was at the turn's last model call; arrives just before
    /// its [`Message::Usage`], and only when the result names the model's window.
    Context(ContextFill),
    /// Claude Code reported the state of the plan's rate limits.
    RateLimit(RateLimit),
    /// Claude Code asks permission to run a tool.
    PermissionRequest(PermissionAsk),
    /// A `can_use_tool` request that cannot be evaluated; the adapter must deny it.
    InvalidPermissionRequest {
        /// Id to echo in the deny response.
        request_id: String,
    },
}

/// A `can_use_tool` control request, before tool-name mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionAsk {
    /// Id to echo in the control response.
    pub request_id: String,
    /// Claude Code's own tool name.
    pub tool_name: String,
    /// The tool's arguments, as given.
    pub input: Value,
    /// What Claude Code says the call is for, when it says.
    pub description: Option<String>,
}

/// Something skipped while parsing; never carries message content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diagnostic {
    /// The line was not valid JSON.
    MalformedLine {
        /// 1-based line number in the stream.
        line: u64,
    },
    /// The message `type` is not one coda handles.
    UnknownType {
        /// 1-based line number in the stream.
        line: u64,
        /// The unrecognized type.
        kind: String,
    },
    /// A control request whose subtype coda does not answer.
    UnsupportedRequest {
        /// 1-based line number in the stream.
        line: u64,
        /// The unrecognized subtype.
        subtype: String,
    },
    /// A record missing a field coda needs, or carrying a value coda cannot read.
    MalformedRequest {
        /// 1-based line number in the stream.
        line: u64,
        /// What was missing or unreadable.
        reason: &'static str,
    },
    /// A Codex thread id that is missing or not a plain identifier.
    InvalidThreadId {
        /// 1-based line number in the stream.
        line: u64,
    },
    /// The line exceeded the size limit and was discarded.
    LineTooLong {
        /// 1-based line number in the stream.
        line: u64,
    },
}

/// Turns raw output bytes into [`Message`]s, tolerating bad input.
#[derive(Debug, Default)]
pub struct StreamParser {
    framer: LineFramer,
    log: DiagnosticLog,
}

impl StreamParser {
    /// An empty parser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds bytes as read; a line split across calls is held until complete.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Message> {
        let mut messages = Vec::new();
        for framed in self.framer.feed(bytes) {
            match framed {
                Framed::Line { number, bytes } => messages.extend(self.parse_bytes(&bytes, number)),
                Framed::TooLong { number } => self.record(Diagnostic::LineTooLong { line: number }),
            }
        }
        messages
    }

    /// The first diagnostics recorded, in stream order.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        self.log.items()
    }

    /// How many diagnostics were dropped after the retention cap was reached.
    #[must_use]
    pub fn dropped_diagnostics(&self) -> u64 {
        self.log.dropped()
    }

    fn record(&mut self, diagnostic: Diagnostic) {
        self.log.record(diagnostic);
    }

    fn parse_bytes(&mut self, bytes: &[u8], line: u64) -> Vec<Message> {
        let text = String::from_utf8_lossy(bytes);
        if text.trim().is_empty() {
            return Vec::new();
        }
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            self.record(Diagnostic::MalformedLine { line });
            return Vec::new();
        };
        result_usage(&value)
            .into_iter()
            .chain(self.parse_line(&value, line))
            .chain(tool_markers(&value))
            .collect()
    }

    fn parse_line(&mut self, value: &Value, line: u64) -> Option<Message> {
        let kind = value["type"].as_str().unwrap_or_default();
        match kind {
            "system" if value["subtype"] == "init" => {
                let Some(session_id) = value["session_id"].as_str() else {
                    self.record(Diagnostic::MalformedRequest {
                        line,
                        reason: "init without session_id",
                    });
                    return None;
                };
                Some(Message::Init {
                    session_id: session_id.to_owned(),
                    api_key_source: value["apiKeySource"].as_str().map(str::to_owned),
                })
            }
            // Real streams also carry the model's own tool results.
            "system" | "user" => None,
            "rate_limit_event" => {
                let mut unusable = Vec::new();
                let limit = rate_limit(&value["rate_limit_info"], &mut unusable);
                unusable.sort_unstable();
                unusable.dedup();
                for reason in unusable {
                    self.record(Diagnostic::MalformedRequest { line, reason });
                }
                limit.map(Message::RateLimit)
            }
            "stream_event" => text_delta(value).map(Message::TextDelta),
            "assistant" => Some(Message::Assistant {
                text: assistant_text(value),
            }),
            "result" => Some(Message::Result {
                is_error: value["is_error"].as_bool().unwrap_or(false),
            }),
            "control_request" => self.permission_request(value, line),
            other => {
                self.record(Diagnostic::UnknownType {
                    line,
                    kind: bounded(other),
                });
                None
            }
        }
    }

    fn permission_request(&mut self, value: &Value, line: u64) -> Option<Message> {
        let request = &value["request"];
        let subtype = request["subtype"].as_str().unwrap_or_default();
        if subtype != CAN_USE_TOOL {
            self.record(Diagnostic::UnsupportedRequest {
                line,
                subtype: bounded(subtype),
            });
            return None;
        }
        let Some(request_id) = value["request_id"].as_str() else {
            self.record(Diagnostic::MalformedRequest {
                line,
                reason: "can_use_tool without request_id",
            });
            return None;
        };
        let Some(tool_name) = request["tool_name"].as_str() else {
            self.record(Diagnostic::MalformedRequest {
                line,
                reason: "can_use_tool without tool_name",
            });
            return Some(Message::InvalidPermissionRequest {
                request_id: request_id.to_owned(),
            });
        };
        Some(Message::PermissionRequest(PermissionAsk {
            request_id: request_id.to_owned(),
            tool_name: tool_name.to_owned(),
            input: request["input"].clone(),
            description: request["description"].as_str().map(str::to_owned),
        }))
    }
}

fn text_delta(value: &Value) -> Option<String> {
    let event = &value["event"];
    if event["type"] != "content_block_delta" {
        return None;
    }
    event["delta"]["text"].as_str().map(str::to_owned)
}

/// The tool calls an assistant message starts and a user message finishes.
fn tool_markers(value: &Value) -> Vec<Message> {
    let (block_type, started) = match value["type"].as_str() {
        Some("assistant") => ("tool_use", true),
        Some("user") => ("tool_result", false),
        _ => return Vec::new(),
    };
    value["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == block_type)
        .filter_map(|block| {
            if started {
                let CallInfo { name, summary } = claude_call(block);
                Some(Message::ToolStarted {
                    id: block["id"].as_str()?.to_owned(),
                    name,
                    summary,
                })
            } else {
                Some(Message::ToolFinished {
                    id: block["tool_use_id"].as_str()?.to_owned(),
                    failed: block["is_error"].as_bool().unwrap_or(false),
                })
            }
        })
        .collect()
}

fn result_usage(value: &Value) -> Vec<Message> {
    if value["type"] != "result" {
        return Vec::new();
    }
    let Some(usage) = value["usage"].as_object() else {
        return Vec::new();
    };
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let total = TokenUsage {
        input_tokens: count("input_tokens"),
        output_tokens: count("output_tokens"),
        cache_read_tokens: count("cache_read_input_tokens"),
        cache_write_tokens: count("cache_creation_input_tokens"),
    };
    let mut messages: Vec<Message> = context_fill(value, total)
        .map(Message::Context)
        .into_iter()
        .collect();
    messages.push(Message::Usage(total));
    messages
}

/// How full the context was. The result's `usage` adds up every model call of the
/// turn, so the last entry of `iterations` is read when there is one. The window
/// is the largest `contextWindow` among the models that ran.
fn context_fill(result: &Value, total: TokenUsage) -> Option<ContextFill> {
    let window = result["modelUsage"]
        .as_object()?
        .values()
        .filter_map(|model| model["contextWindow"].as_u64())
        .filter_map(|window| u32::try_from(window).ok())
        .max()
        .filter(|window| *window > 0)?;
    let held = |usage: &Value| {
        [
            "input_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ]
        .iter()
        .map(|key| usage[key].as_u64().unwrap_or(0))
        .fold(0_u64, u64::saturating_add)
    };
    let used_tokens = match result["usage"]["iterations"]
        .as_array()
        .and_then(|calls| calls.last())
    {
        Some(last) => held(last),
        None => total
            .input_tokens
            .saturating_add(total.cache_read_tokens)
            .saturating_add(total.cache_write_tokens),
    };
    Some(ContextFill {
        used_tokens,
        window_tokens: window,
    })
}

/// Reads a rate-limit report; `unusable` collects what was present but could not be read.
fn rate_limit(info: &Value, unusable: &mut Vec<&'static str>) -> Option<RateLimit> {
    let text = |key: &str| info[key].as_str().map(str::to_owned);
    let mut windows: Vec<_> = info["unifiedWindows"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, window)| {
            let Some(utilization) = window["utilization"].as_f64() else {
                unusable.push(WINDOW_WITHOUT_UTILIZATION);
                return None;
            };
            Some(RateLimitWindow {
                name: name.clone(),
                utilization_bps: basis_points(utilization),
                resets_at: epoch_seconds(&window["resetsAt"], unusable),
            })
        })
        .collect();
    windows.sort_by(|a, b| a.name.cmp(&b.name));
    Some(RateLimit {
        status: text("status")?,
        window_type: text("rateLimitType"),
        resets_at: epoch_seconds(&info["resetsAt"], unusable),
        windows,
        is_using_overage: info["isUsingOverage"].as_bool().unwrap_or(false),
        overage_status: text("overageStatus"),
        overage_resets_at: epoch_seconds(&info["overageResetsAt"], unusable),
        overage_disabled_reason: text("overageDisabledReason"),
    })
}

/// Reads a reset time as whole seconds since the Unix epoch.
///
/// A whole-number float is accepted. A negative, fractional or too large value
/// (milliseconds, for one) is not guessed at: it reads as absent and is noted in `unusable`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn epoch_seconds(value: &Value, unusable: &mut Vec<&'static str>) -> Option<u64> {
    if value.is_null() {
        return None;
    }
    let seconds = value
        .as_u64()
        .or_else(|| {
            let float = value.as_f64()?;
            // The range check keeps the cast exact.
            (float >= 0.0 && float.fract() == 0.0 && float <= MAX_EPOCH_SECONDS as f64)
                .then_some(float as u64)
        })
        .filter(|seconds| *seconds <= MAX_EPOCH_SECONDS);
    if seconds.is_none() {
        unusable.push(UNUSABLE_RESET_TIME);
    }
    seconds
}

/// The clamp keeps the value inside `u32`, so the cast cannot truncate or change sign.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn basis_points(fraction: f64) -> u32 {
    (fraction * BASIS_POINTS_PER_UNIT)
        .round()
        .clamp(0.0, f64::from(u32::MAX)) as u32
}

fn assistant_text(value: &Value) -> String {
    value["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect()
}

/// Maps a Claude Code tool call to the normalized request the policy sees.
#[must_use]
pub fn map_tool(name: &str, input: &Value, cwd: &Path) -> ToolRequest {
    let path_of = |key: &str| input[key].as_str().map(PathBuf::from).into_iter().collect();
    let (tool, target_paths, command) = match name {
        "Bash" => (
            Tool::Bash,
            Vec::new(),
            input["command"].as_str().map(str::to_owned),
        ),
        "Edit" | "Write" | "MultiEdit" => (Tool::Edit, path_of("file_path"), None),
        "Read" => (Tool::Read, path_of("file_path"), None),
        "Grep" | "Glob" => {
            let mut paths: Vec<PathBuf> = path_of("path");
            if paths.is_empty() {
                paths.push(cwd.to_path_buf());
            }
            (Tool::Grep, paths, None)
        }
        other => (Tool::Unknown(other.to_owned()), Vec::new(), None),
    };
    ToolRequest {
        tool,
        target_paths,
        command,
        cwd: cwd.to_path_buf(),
        reason: None,
    }
}

/// The one-line `control_response` that answers a `can_use_tool` request.
#[must_use]
pub fn control_response(request_id: &str, decision: PermissionDecision, input: &Value) -> String {
    let verdict = match decision {
        PermissionDecision::AllowOnce | PermissionDecision::AllowAlways => {
            json!({"behavior": "allow", "updatedInput": input})
        }
        _ => json!({"behavior": "deny", "message": DENY_MESSAGE}),
    };
    json!({
        "type": "control_response",
        "response": {"subtype": "success", "request_id": request_id, "response": verdict},
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonl::{MAX_DIAGNOSTICS, MAX_KIND_CHARS, MAX_LINE_BYTES};
    use crate::tool_request::Tool;
    use coda_protocol::types::PermissionDecision;
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};

    // Synthetic fixtures: hand-written, not recorded from a live session.
    const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"sess-1"}"#;
    const DELTA: &str = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}}"#;
    const ASK: &str = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}"#;
    const RESULT: &str = r#"{"type":"result","is_error":true}"#;

    fn feed_lines(parser: &mut StreamParser, lines: &[&str]) -> Vec<Message> {
        let mut input = lines.join("\n");
        input.push('\n');
        parser.feed(input.as_bytes())
    }

    // Field names and shapes as claude 2.1.296 wrote them (see live-text.jsonl).
    const RATE_LIMIT: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1794193200,"rateLimitType":"seven_day","overageStatus":"rejected","overageDisabledReason":"out_of_credits","overageResetsAt":1796083200,"isUsingOverage":true,"unifiedWindows":{"seven_day":{"utilization":0.0912,"resetsAt":1794193200}}}}"#;
    const RESULT_WITH_USAGE: &str = r#"{"type":"result","is_error":false,"usage":{"input_tokens":3,"cache_creation_input_tokens":5,"cache_read_input_tokens":7,"output_tokens":11}}"#;

    #[test]
    fn a_rate_limit_event_is_parsed_with_utilization_in_basis_points() {
        // Arrange
        let mut parser = StreamParser::new();

        // Act
        let messages = feed_lines(&mut parser, &[RATE_LIMIT]);

        // Assert
        let expected = RateLimit {
            status: "allowed_warning".to_owned(),
            window_type: Some("seven_day".to_owned()),
            resets_at: Some(1_794_193_200),
            windows: vec![RateLimitWindow {
                name: "seven_day".to_owned(),
                utilization_bps: 912,
                resets_at: Some(1_794_193_200),
            }],
            is_using_overage: true,
            overage_status: Some("rejected".to_owned()),
            overage_resets_at: Some(1_796_083_200),
            overage_disabled_reason: Some("out_of_credits".to_owned()),
        };
        assert_eq!(messages, vec![Message::RateLimit(expected)]);
        assert_eq!(parser.diagnostics(), []);
    }

    fn parse_limit(info: &Value) -> (Option<RateLimit>, Vec<&'static str>) {
        let mut unusable = Vec::new();
        let limit = rate_limit(info, &mut unusable);
        (limit, unusable)
    }

    #[test]
    fn utilization_converts_to_basis_points_and_may_pass_one_hundred_percent() {
        let cases = [
            (0.0, 0),
            (0.0001, 1),
            (1.0, 10_000),
            (1.5, 15_000),
            (-0.5, 0),
            (1e30, u32::MAX),
        ];

        for (fraction, expected) in cases {
            assert_eq!(basis_points(fraction), expected, "{fraction}");
        }
    }

    #[test]
    fn a_window_without_a_numeric_utilization_is_dropped_and_noted() {
        // Arrange
        let info = json!({"status":"allowed","unifiedWindows":{
            "five_hour":{"resetsAt":1_794_193_200},
            "seven_day":{"utilization":null,"resetsAt":1_794_193_200},
            "monthly":{"utilization":0.5,"resetsAt":1_794_193_200}}});

        // Act
        let (limit, unusable) = parse_limit(&info);

        // Assert
        let names: Vec<_> = limit.unwrap().windows.into_iter().map(|w| w.name).collect();
        assert_eq!(names, ["monthly"]);
        assert_eq!(unusable, [WINDOW_WITHOUT_UTILIZATION; 2]);
    }

    #[test]
    fn reset_times_are_whole_seconds_and_anything_else_is_dropped_and_noted() {
        let cases = [
            (json!(1_794_193_200_u64), Some(1_794_193_200), 0),
            (json!(1_794_193_200.0), Some(1_794_193_200), 0),
            (json!(null), None, 0),
            (json!(1_794_193_200.5), None, 1),
            (json!(-5), None, 1),
            (json!(-5.0), None, 1),
            (json!("soon"), None, 1),
            (json!(1_794_193_200_000_u64), None, 1),
            (json!(1.794_193_2e12), None, 1),
        ];

        for (value, expected, notes) in cases {
            let mut unusable = Vec::new();
            assert_eq!(epoch_seconds(&value, &mut unusable), expected, "{value}");
            assert_eq!(unusable.len(), notes, "{value}");
        }
    }

    #[test]
    fn an_unusable_reset_time_leaves_the_rest_of_the_report_and_is_recorded_with_its_line() {
        // Arrange
        let mut parser = StreamParser::new();
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1794193200000,"unifiedWindows":{"five_hour":{"utilization":1.0}}}}"#;

        // Act
        let messages = feed_lines(&mut parser, &[line]);

        // Assert
        let Message::RateLimit(limit) = &messages[0] else {
            panic!("no rate limit");
        };
        assert_eq!(limit.resets_at, None);
        assert_eq!(limit.windows[0].utilization_bps, 10_000);
        assert_eq!(
            parser.diagnostics(),
            [Diagnostic::MalformedRequest {
                line: 1,
                reason: UNUSABLE_RESET_TIME
            }]
        );
    }

    #[test]
    fn a_rate_limit_event_without_a_status_is_dropped() {
        // Arrange
        let mut parser = StreamParser::new();
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{}}"#;

        // Act
        let messages = feed_lines(&mut parser, &[line]);

        // Assert
        assert_eq!(messages, vec![]);
    }

    #[test]
    fn a_result_reports_its_usage_before_the_turn_ends() {
        // Arrange
        let mut parser = StreamParser::new();

        // Act
        let messages = feed_lines(&mut parser, &[RESULT_WITH_USAGE]);

        // Assert
        let usage = TokenUsage {
            input_tokens: 3,
            output_tokens: 11,
            cache_read_tokens: 7,
            cache_write_tokens: 5,
        };
        assert_eq!(
            messages,
            vec![Message::Usage(usage), Message::Result { is_error: false }]
        );
    }

    #[test]
    fn a_result_with_a_window_reports_the_last_calls_context_before_its_usage() {
        // Arrange
        let mut parser = StreamParser::new();
        let result = r#"{"type":"result","is_error":false,"usage":{"input_tokens":3,"output_tokens":11,"iterations":[{"input_tokens":2,"cache_read_input_tokens":1000,"cache_creation_input_tokens":40},{"input_tokens":1,"cache_read_input_tokens":2000,"cache_creation_input_tokens":9}]},"modelUsage":{"m":{"contextWindow":200000}}}"#;

        // Act
        let messages = feed_lines(&mut parser, &[result]);

        // Assert
        let fill = ContextFill {
            used_tokens: 2010,
            window_tokens: 200_000,
        };
        assert!(
            matches!(messages.as_slice(), [Message::Context(found), Message::Usage(_), Message::Result { .. }] if *found == fill),
            "{messages:?}"
        );
    }

    #[test]
    fn without_iterations_the_context_is_the_turns_input_and_cache() {
        // Arrange
        let mut parser = StreamParser::new();
        let result = r#"{"type":"result","is_error":false,"usage":{"input_tokens":3,"cache_creation_input_tokens":5,"cache_read_input_tokens":7,"output_tokens":11},"modelUsage":{"m":{"contextWindow":1000000}}}"#;

        // Act
        let messages = feed_lines(&mut parser, &[result]);

        // Assert
        let fill = ContextFill {
            used_tokens: 15,
            window_tokens: 1_000_000,
        };
        assert_eq!(messages[0], Message::Context(fill));
    }

    #[test]
    fn the_largest_window_of_the_models_that_ran_is_used() {
        // Arrange
        let mut parser = StreamParser::new();
        let result = r#"{"type":"result","is_error":false,"usage":{"input_tokens":1},"modelUsage":{"a":{"contextWindow":200000},"b":{"contextWindow":1000000},"c":{}}}"#;

        // Act
        let messages = feed_lines(&mut parser, &[result]);

        // Assert
        assert!(matches!(&messages[0], Message::Context(fill) if fill.window_tokens == 1_000_000));
    }

    #[test]
    fn a_missing_or_zero_window_adds_no_context_message() {
        for model_usage in [
            r#""modelUsage":{"m":{}}"#,
            r#""modelUsage":{"m":{"contextWindow":0}}"#,
            r#""modelUsage":{}"#,
            r#""other":1"#,
        ] {
            // Arrange
            let mut parser = StreamParser::new();
            let result = format!(
                r#"{{"type":"result","is_error":false,"usage":{{"input_tokens":1}},{model_usage}}}"#
            );

            // Act
            let messages = feed_lines(&mut parser, &[&result]);

            // Assert
            assert!(
                !messages.iter().any(|m| matches!(m, Message::Context(_))),
                "{model_usage}"
            );
        }
    }

    #[test]
    fn a_result_without_usage_adds_no_usage_message() {
        // Arrange
        let mut parser = StreamParser::new();

        // Act
        let messages = feed_lines(&mut parser, &[RESULT]);

        // Assert
        assert_eq!(messages, vec![Message::Result { is_error: true }]);
    }

    #[test]
    fn known_message_types_are_parsed() {
        // Arrange
        let mut parser = StreamParser::new();

        // Act
        let messages = feed_lines(&mut parser, &[INIT, DELTA, ASK, RESULT]);

        // Assert
        assert_eq!(
            messages,
            vec![
                Message::Init {
                    session_id: "sess-1".to_owned(),
                    api_key_source: None,
                },
                Message::TextDelta("hi".to_owned()),
                Message::PermissionRequest(PermissionAsk {
                    request_id: "r1".to_owned(),
                    tool_name: "Bash".to_owned(),
                    input: json!({"command":"ls"}),
                    description: None,
                }),
                Message::Result { is_error: true },
            ]
        );
        assert_eq!(parser.diagnostics(), []);
    }

    #[test]
    fn the_init_frame_carries_where_the_api_key_came_from() {
        // Arrange
        let mut parser = StreamParser::new();
        let with_key = r#"{"type":"system","subtype":"init","session_id":"s","apiKeySource":"ANTHROPIC_API_KEY"}"#;
        let without =
            r#"{"type":"system","subtype":"init","session_id":"s","apiKeySource":"none"}"#;
        let odd = r#"{"type":"system","subtype":"init","session_id":"s","apiKeySource":7}"#;

        // Act
        let messages = feed_lines(&mut parser, &[with_key, without, odd]);

        // Assert
        let sources: Vec<Option<String>> = messages
            .into_iter()
            .map(|message| match message {
                Message::Init { api_key_source, .. } => api_key_source,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            sources,
            vec![
                Some("ANTHROPIC_API_KEY".to_owned()),
                Some("none".to_owned()),
                None
            ]
        );
    }

    #[test]
    fn a_malformed_line_is_recorded_and_parsing_continues() {
        // Arrange
        let mut parser = StreamParser::new();

        // Act
        let messages = feed_lines(&mut parser, &["{not json", RESULT]);

        // Assert
        assert_eq!(messages, vec![Message::Result { is_error: true }]);
        assert_eq!(
            parser.diagnostics(),
            [Diagnostic::MalformedLine { line: 1 }]
        );
    }

    #[test]
    fn a_line_split_across_reads_is_reassembled() {
        // Arrange
        let mut parser = StreamParser::new();
        let (head, tail) = INIT.split_at(20);

        // Act
        let first = parser.feed(head.as_bytes());
        let second = parser.feed(format!("{tail}\n").as_bytes());

        // Assert
        assert_eq!(first, []);
        assert_eq!(
            second,
            vec![Message::Init {
                session_id: "sess-1".to_owned(),
                api_key_source: None,
            }]
        );
    }

    #[test]
    fn an_unknown_message_type_is_recorded_and_parsing_continues() {
        // Arrange
        let mut parser = StreamParser::new();

        // Act
        let messages = feed_lines(&mut parser, &[r#"{"type":"future_event"}"#, RESULT]);

        // Assert
        assert_eq!(messages.len(), 1);
        assert_eq!(
            parser.diagnostics(),
            [Diagnostic::UnknownType {
                line: 1,
                kind: "future_event".to_owned()
            }]
        );
    }

    #[test]
    fn a_stream_event_without_text_is_ignored_without_a_diagnostic() {
        // Arrange
        let mut parser = StreamParser::new();
        let start = r#"{"type":"stream_event","event":{"type":"message_start"}}"#;

        // Act
        let messages = feed_lines(&mut parser, &[start]);

        // Assert
        assert_eq!(messages, []);
        assert_eq!(parser.diagnostics(), []);
    }

    #[test]
    fn an_assistant_message_yields_its_text_blocks() {
        // Arrange
        let mut parser = StreamParser::new();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"a"},{"type":"tool_use","name":"Bash"},{"type":"text","text":"b"}]}}"#;

        // Act
        let messages = feed_lines(&mut parser, &[line]);

        // Assert
        assert_eq!(
            messages,
            vec![Message::Assistant {
                text: "ab".to_owned()
            }]
        );
    }

    #[test]
    fn a_tool_use_starts_a_tool_and_its_result_finishes_it() {
        // Arrange
        let mut parser = StreamParser::new();
        let started = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la\nmore"}}]}}"#;
        let finished = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true}]}}"#;

        // Act
        let messages = feed_lines(&mut parser, &[started, finished]);

        // Assert
        assert_eq!(
            messages,
            vec![
                Message::Assistant {
                    text: String::new()
                },
                Message::ToolStarted {
                    id: "t1".to_owned(),
                    name: "Bash".to_owned(),
                    summary: Some("ls -la".to_owned()),
                },
                Message::ToolFinished {
                    id: "t1".to_owned(),
                    failed: true,
                },
            ]
        );
    }

    #[test]
    fn a_control_request_of_another_subtype_is_recorded() {
        // Arrange
        let mut parser = StreamParser::new();
        let line =
            r#"{"type":"control_request","request_id":"r2","request":{"subtype":"hook_callback"}}"#;

        // Act
        let messages = feed_lines(&mut parser, &[line]);

        // Assert
        assert_eq!(messages, []);
        assert_eq!(
            parser.diagnostics(),
            [Diagnostic::UnsupportedRequest {
                line: 1,
                subtype: "hook_callback".to_owned()
            }]
        );
    }

    #[test]
    fn a_line_past_the_limit_is_discarded_and_recorded_without_content() {
        // Arrange
        let mut parser = StreamParser::new();
        let chunk = vec![b'a'; MAX_LINE_BYTES / 2 + 1];

        // Act
        let mut messages = parser.feed(&chunk);
        messages.extend(parser.feed(&chunk));
        messages.extend(parser.feed(b"tail\n"));
        messages.extend(feed_lines(&mut parser, &[RESULT]));

        // Assert
        assert_eq!(messages, vec![Message::Result { is_error: true }]);
        assert_eq!(parser.diagnostics(), [Diagnostic::LineTooLong { line: 1 }]);
    }

    #[test]
    fn retained_diagnostics_are_capped_and_the_overflow_counted() {
        // Arrange
        let mut parser = StreamParser::new();
        let junk = vec!["{bad"; MAX_DIAGNOSTICS + 5];

        // Act
        feed_lines(&mut parser, &junk);

        // Assert
        assert_eq!(parser.diagnostics().len(), MAX_DIAGNOSTICS);
        assert_eq!(parser.dropped_diagnostics(), 5);
    }

    #[test]
    fn an_unknown_type_is_truncated_in_the_diagnostic() {
        // Arrange
        let mut parser = StreamParser::new();
        let line = format!(r#"{{"type":"{}"}}"#, "x".repeat(MAX_KIND_CHARS * 4));

        // Act
        feed_lines(&mut parser, &[&line]);

        // Assert
        assert_eq!(
            parser.diagnostics(),
            [Diagnostic::UnknownType {
                line: 1,
                kind: "x".repeat(MAX_KIND_CHARS)
            }]
        );
    }

    #[test]
    fn a_permission_request_without_a_tool_name_is_flagged_for_denial() {
        // Arrange
        let mut parser = StreamParser::new();
        let line =
            r#"{"type":"control_request","request_id":"r9","request":{"subtype":"can_use_tool"}}"#;

        // Act
        let messages = feed_lines(&mut parser, &[line]);

        // Assert
        assert_eq!(
            messages,
            vec![Message::InvalidPermissionRequest {
                request_id: "r9".to_owned()
            }]
        );
        assert_eq!(
            parser.diagnostics(),
            [Diagnostic::MalformedRequest {
                line: 1,
                reason: "can_use_tool without tool_name"
            }]
        );
    }

    #[test]
    fn an_init_without_a_session_id_is_recorded() {
        // Arrange
        let mut parser = StreamParser::new();

        // Act
        let messages = feed_lines(&mut parser, &[r#"{"type":"system","subtype":"init"}"#]);

        // Assert
        assert_eq!(messages, []);
        assert_eq!(
            parser.diagnostics(),
            [Diagnostic::MalformedRequest {
                line: 1,
                reason: "init without session_id"
            }]
        );
    }

    fn map(name: &str, input: &Value) -> ToolRequest {
        map_tool(name, input, Path::new("/work"))
    }

    #[test]
    fn bash_maps_to_a_command_request() {
        // Arrange
        let input = json!({"command":"ls -l"});

        // Act
        let request = map("Bash", &input);

        // Assert
        assert_eq!(request.tool, Tool::Bash);
        assert_eq!(request.command.as_deref(), Some("ls -l"));
        assert_eq!(request.cwd, PathBuf::from("/work"));
        assert_eq!(request.target_paths, [] as [PathBuf; 0]);
    }

    #[test]
    fn file_changing_tools_map_to_edit_with_the_target_path() {
        for name in ["Edit", "Write", "MultiEdit"] {
            // Arrange
            let input = json!({"file_path":"/work/a.rs"});

            // Act
            let request = map(name, &input);

            // Assert
            assert_eq!(request.tool, Tool::Edit, "{name}");
            assert_eq!(request.target_paths, [PathBuf::from("/work/a.rs")]);
        }
    }

    #[test]
    fn read_maps_to_read_with_the_target_path() {
        // Arrange
        let input = json!({"file_path":"/work/a.rs"});

        // Act
        let request = map("Read", &input);

        // Assert
        assert_eq!(request.tool, Tool::Read);
        assert_eq!(request.target_paths, [PathBuf::from("/work/a.rs")]);
    }

    #[test]
    fn search_tools_map_to_grep_defaulting_the_path_to_the_cwd() {
        for (name, input, paths) in [
            (
                "Grep",
                json!({"pattern":"x","path":"/work/src"}),
                vec!["/work/src"],
            ),
            ("Glob", json!({"pattern":"*.rs"}), vec!["/work"]),
        ] {
            // Act
            let request = map(name, &input);

            // Assert
            assert_eq!(request.tool, Tool::Grep, "{name}");
            let expected: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
            assert_eq!(request.target_paths, expected);
        }
    }

    #[test]
    fn any_other_tool_maps_to_unknown_with_its_own_name() {
        // Arrange
        let input = json!({"url":"https://example.test"});

        // Act
        let request = map("WebFetch", &input);

        // Assert
        assert_eq!(request.tool, Tool::Unknown("WebFetch".to_owned()));
        assert_eq!(request.target_paths, [] as [PathBuf; 0]);
        assert_eq!(request.command, None);
    }

    fn response(decision: PermissionDecision) -> Value {
        let line = control_response("r1", decision, &json!({"command":"ls"}));
        assert!(!line.contains('\n'));
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn allow_decisions_write_the_allow_shape_echoing_the_input() {
        for decision in [
            PermissionDecision::AllowOnce,
            PermissionDecision::AllowAlways,
        ] {
            // Act
            let value = response(decision);

            // Assert
            assert_eq!(
                value,
                json!({"type":"control_response","response":{"subtype":"success","request_id":"r1",
                    "response":{"behavior":"allow","updatedInput":{"command":"ls"}}}})
            );
        }
    }

    #[test]
    fn deny_writes_the_deny_shape() {
        // Act
        let value = response(PermissionDecision::Deny);

        // Assert
        assert_eq!(
            value,
            json!({"type":"control_response","response":{"subtype":"success","request_id":"r1",
                "response":{"behavior":"deny","message":"denied by coda"}}})
        );
    }
}

#[cfg(all(test, unix))]
mod fake_turn_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    const USER_MESSAGE: &str = r#"{"type":"user","message":{"role":"user","content":"run ls"}}"#;

    struct Turn {
        stdout: String,
        messages: Vec<Message>,
    }

    /// Runs `fake-claude` through one permission-gated turn and collects its output.
    fn run_turn(decision: PermissionDecision, api_key: Option<&str>) -> Turn {
        let mut command = Command::new(coda_testkit::paths::fake_claude_path().unwrap());
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        command.env_remove("ANTHROPIC_API_KEY");
        if let Some(key) = api_key {
            command.env("ANTHROPIC_API_KEY", key);
        }
        let mut child = command.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        writeln!(stdin, "{USER_MESSAGE}").unwrap();

        let mut parser = StreamParser::new();
        let mut raw = Vec::new();
        let mut messages = Vec::new();
        let mut chunk = [0_u8; 256];
        let mut answered = false;
        while !messages.iter().any(|m| matches!(m, Message::Result { .. })) {
            let read = stdout.read(&mut chunk).unwrap();
            assert!(
                read > 0,
                "fake-claude ended before the turn finished: {:?}",
                child.wait()
            );
            raw.extend_from_slice(&chunk[..read]);
            messages.extend(parser.feed(&chunk[..read]));
            if !answered
                && let Some(Message::PermissionRequest(ask)) = messages
                    .iter()
                    .find(|m| matches!(m, Message::PermissionRequest(_)))
            {
                writeln!(
                    stdin,
                    "{}",
                    control_response(&ask.request_id, decision, &ask.input)
                )
                .unwrap();
                answered = true;
            }
        }
        drop(stdin);
        child.wait().unwrap();
        Turn {
            stdout: String::from_utf8(raw).unwrap(),
            messages,
        }
    }

    #[test]
    fn an_allowed_request_lets_the_turn_complete() {
        // Act
        let turn = run_turn(PermissionDecision::AllowOnce, None);

        // Assert
        assert!(turn.messages.contains(&Message::Assistant {
            text: "ran".to_owned()
        }));
        assert_eq!(
            turn.messages.last(),
            Some(&Message::Result { is_error: false })
        );
    }

    #[test]
    fn a_denied_request_is_reported_by_the_turn() {
        // Act
        let turn = run_turn(PermissionDecision::Deny, None);

        // Assert
        assert!(turn.messages.contains(&Message::Assistant {
            text: "denied".to_owned()
        }));
    }

    #[test]
    fn the_session_id_and_stream_text_are_parsed_from_the_fake() {
        // Act
        let turn = run_turn(PermissionDecision::AllowOnce, None);

        // Assert
        assert!(matches!(turn.messages.first(), Some(Message::Init { .. })));
        assert!(
            turn.messages
                .iter()
                .any(|m| matches!(m, Message::TextDelta(_)))
        );
    }

    #[test]
    fn the_fake_reports_whether_the_api_key_reached_it() {
        // Act
        let without = run_turn(PermissionDecision::Deny, None);
        let with = run_turn(PermissionDecision::Deny, Some("sk-test"));

        // Assert
        assert!(
            without
                .stdout
                .contains(r#""anthropic_api_key_present":false"#)
        );
        assert!(with.stdout.contains(r#""anthropic_api_key_present":true"#));
    }
}
