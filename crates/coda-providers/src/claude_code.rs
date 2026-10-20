// SPDX-License-Identifier: AGPL-3.0-only

//! Claude Code stream-json parsing and tool-name mapping.

use std::path::{Path, PathBuf};

use coda_protocol::types::PermissionDecision;
use serde_json::{Value, json};

use crate::jsonl::{DiagnosticLog, Framed, LineFramer, bounded};
use crate::tool_request::{Tool, ToolRequest};

const CAN_USE_TOOL: &str = "can_use_tool";
const DENY_MESSAGE: &str = "denied by coda";

/// One message from the Claude Code output stream that coda acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// The session started; carries the id used with `--resume`.
    Init {
        /// Claude Code's session id.
        session_id: String,
    },
    /// A streamed fragment of model text.
    TextDelta(String),
    /// A complete assistant message, text blocks joined.
    Assistant {
        /// The message text.
        text: String,
    },
    /// The turn finished.
    Result {
        /// Whether the turn ended in an error.
        is_error: bool,
    },
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
    /// A record missing a field coda needs.
    MalformedRequest {
        /// 1-based line number in the stream.
        line: u64,
        /// What was missing.
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

    fn parse_bytes(&mut self, bytes: &[u8], line: u64) -> Option<Message> {
        let text = String::from_utf8_lossy(bytes);
        if text.trim().is_empty() {
            return None;
        }
        self.parse_line(&text, line)
    }

    fn parse_line(&mut self, text: &str, line: u64) -> Option<Message> {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            self.record(Diagnostic::MalformedLine { line });
            return None;
        };
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
                })
            }
            // Real streams also carry the model's own tool results and rate-limit updates.
            "system" | "user" | "rate_limit_event" => None,
            "stream_event" => text_delta(&value).map(Message::TextDelta),
            "assistant" => Some(Message::Assistant {
                text: assistant_text(&value),
            }),
            "result" => Some(Message::Result {
                is_error: value["is_error"].as_bool().unwrap_or(false),
            }),
            "control_request" => self.permission_request(&value, line),
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
                    session_id: "sess-1".to_owned()
                },
                Message::TextDelta("hi".to_owned()),
                Message::PermissionRequest(PermissionAsk {
                    request_id: "r1".to_owned(),
                    tool_name: "Bash".to_owned(),
                    input: json!({"command":"ls"}),
                }),
                Message::Result { is_error: true },
            ]
        );
        assert_eq!(parser.diagnostics(), []);
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
                session_id: "sess-1".to_owned()
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
