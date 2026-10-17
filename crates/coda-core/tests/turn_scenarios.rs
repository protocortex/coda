// SPDX-License-Identifier: AGPL-3.0-only

//! Turn loop scenarios that exercise the budget, a blocked tool process and
//! provider failures through the public API.

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coda_core::prompt::{Conversation, Message, Prefix, ToolSchema};
use coda_core::turn::{StopCondition, ToolSet, TurnConfig, TurnError, TurnEvent, TurnLoop};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision, SessionId};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{
    ModelEvent, ModelProvider, ModelRequest, ModelStream, ProviderError, RequestMessage,
    StopReason, Usage,
};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store};
use coda_tools::bash::{BashConfig, BashTool};
use coda_tools::{Tool, ToolError, ToolOutput};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;

const BOUND: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);
const LIMIT: u64 = 10;

struct AllowAll;

impl PermissionGate for AllowAll {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::AllowOnce)
    }
}

/// A faux provider that remembers every request it was sent.
struct Recording {
    inner: FauxProvider,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl ModelProvider for Recording {
    fn stream(
        &self,
        request: ModelRequest,
    ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.stream(request)
    }
}

/// A tool that always returns the same text.
struct Canned;

impl Tool for Canned {
    fn name(&self) -> &'static str {
        "canned"
    }

    fn call(&self, _input: &Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            text: "ran".to_owned(),
            truncated: false,
        })
    }
}

struct Fixture {
    dir: TempDir,
    store: Arc<Mutex<Store>>,
    session: SessionId,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("data")).unwrap();
        let session = store
            .create_session(&NewSession {
                backend: BackendKind::ClaudeCode,
                model: None,
                cwd: "/ws".to_owned(),
                mode: Mode::default(),
            })
            .unwrap();
        std::fs::create_dir(dir.path().join("ws")).unwrap();
        Self {
            dir,
            store: Arc::new(Mutex::new(store)),
            session,
            requests: Arc::default(),
        }
    }

    fn workdir(&self) -> PathBuf {
        self.dir.path().join("ws")
    }

    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn request(&self, index: usize) -> ModelRequest {
        self.requests.lock().unwrap()[index].clone()
    }

    fn turn_loop(
        &self,
        turns: Vec<Turn>,
        tools: ToolSet,
        token_limit: u64,
    ) -> (TurnLoop<Recording, AllowAll>, UnboundedReceiver<TurnEvent>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let schema = ToolSchema {
            name: "read".into(),
            description: "Read a file.".into(),
            input_schema: json!({"type": "object"}),
        };
        let prefix = Prefix::new("You are Coda.", vec![schema], "Use tabs.").unwrap();
        let provider = Recording {
            inner: FauxProvider::new(turns),
            requests: Arc::clone(&self.requests),
        };
        let config = TurnConfig {
            session: self.session.clone(),
            cwd: self.workdir(),
            max_turns: 8,
            token_limit,
            compact_threshold_bytes: 1024,
        };
        let lp = TurnLoop::new(
            provider,
            AllowAll,
            tools,
            Arc::clone(&self.store),
            Conversation::new(prefix),
            config,
            sender,
        );
        (lp, receiver)
    }

    fn bash(&self, timeout: Duration) -> ToolSet {
        let config = BashConfig {
            timeout,
            max_output_bytes: 4096,
            env: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
        };
        ToolSet::new().with(Arc::new(BashTool::new(self.workdir(), config)))
    }
}

fn usage(input_tokens: u64, output_tokens: u64) -> Usage {
    Usage {
        input_tokens,
        output_tokens,
    }
}

fn end_turn(used: Usage) -> Turn {
    Turn {
        events: vec![
            ModelEvent::TextDelta {
                text: "done".into(),
            },
            ModelEvent::Usage(used),
            ModelEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ],
        error: None,
    }
}

fn tool_turn(calls: &[(&str, &str, &str)], used: Usage) -> Turn {
    let mut events = Vec::new();
    for (id, name, arguments) in calls {
        events.push(ModelEvent::ToolCallStart {
            call_id: (*id).into(),
            name: (*name).into(),
        });
        events.push(ModelEvent::ToolCallArgumentsDelta {
            call_id: (*id).into(),
            fragment: (*arguments).into(),
        });
    }
    events.push(ModelEvent::Usage(used));
    events.push(ModelEvent::Stop {
        reason: StopReason::ToolUse,
    });
    Turn {
        events,
        error: None,
    }
}

fn failing(error: ProviderError) -> Turn {
    Turn {
        events: Vec::new(),
        error: Some(error),
    }
}

/// Asserts a user message starts every exchange, every call has exactly one
/// result straight after it, and the history ends on the assistant.
fn assert_valid_history(messages: &[Message]) {
    let mut open = false;
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::User(_) => {
                assert!(!open, "unexpected user message at {index}");
                open = true;
            }
            Message::Assistant { tool_calls, .. } => {
                assert!(open, "unexpected assistant message at {index}");
                for call in tool_calls {
                    index += 1;
                    let found = messages.get(index);
                    assert!(
                        matches!(found, Some(Message::ToolResult { call_id, .. }) if *call_id == call.id),
                        "call {} unanswered, got {found:?}",
                        call.id
                    );
                }
                open = !tool_calls.is_empty();
            }
            Message::ToolResult { .. } => panic!("stray tool result at {index}"),
        }
        index += 1;
    }
    assert!(!open, "history must end on a plain assistant message");
}

fn tool_result(request: &ModelRequest) -> (String, bool) {
    match request.messages.last() {
        Some(RequestMessage::ToolResult { text, is_error, .. }) => (text.clone(), *is_error),
        other => panic!("expected a tool result last, got {other:?}"),
    }
}

async fn poll_until(mut done: impl FnMut() -> bool) {
    tokio::time::timeout(BOUND, async {
        while !done() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("condition not reached in time");
}

fn process_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

async fn read_pid(path: &Path) -> String {
    poll_until(|| std::fs::read_to_string(path).is_ok_and(|s| s.ends_with('\n'))).await;
    std::fs::read_to_string(path).unwrap().trim().to_owned()
}

#[tokio::test]
async fn a_tool_blocked_on_a_fifo_is_killed_at_its_timeout() {
    // Arrange
    let fixture = Fixture::new();
    let command = json!({"command": "mkfifo p; cat p & echo $! > pid; wait"}).to_string();
    let (mut lp, _events) = fixture.turn_loop(
        vec![
            tool_turn(&[("c1", "bash", &command)], usage(1, 1)),
            end_turn(usage(1, 1)),
        ],
        fixture.bash(Duration::from_millis(300)),
        1_000_000,
    );

    // Act
    let stop = tokio::time::timeout(BOUND, lp.send("go"))
        .await
        .unwrap()
        .unwrap();

    // Assert
    assert_eq!(stop, StopCondition::EndTurn);
    let (text, _) = tool_result(&fixture.request(1));
    assert!(text.contains("timed out"), "{text}");
    let reader = read_pid(&fixture.workdir().join("pid")).await;
    poll_until(|| !process_alive(&reader)).await;
}

#[tokio::test]
async fn a_fatal_provider_error_fails_the_send_and_keeps_the_history_valid() {
    // Arrange
    let fixture = Fixture::new();
    let fatal = || {
        failing(ProviderError::Fatal {
            message: "down".into(),
        })
    };
    let (mut lp, _events) = fixture.turn_loop(
        vec![fatal(), fatal(), end_turn(usage(1, 1))],
        ToolSet::new(),
        1_000_000,
    );

    // Act
    let failed = lp.send("one").await;
    let second = lp.send("two").await;

    // Assert
    assert!(
        matches!(
            failed,
            Err(TurnError::Provider(ProviderError::Fatal { .. }))
        ),
        "{failed:?}"
    );
    assert!(second.is_err());
    assert_valid_history(lp.conversation().messages());
    let recovered = lp.send("three").await.unwrap();
    assert_eq!(recovered, StopCondition::EndTurn);
    assert_valid_history(lp.conversation().messages());
}

#[tokio::test]
async fn a_zero_budget_is_refused_before_any_provider_call() {
    // Arrange
    let fixture = Fixture::new();
    let (mut lp, _events) = fixture.turn_loop(vec![end_turn(usage(1, 1))], ToolSet::new(), 0);

    // Act
    let stop = lp.send("go").await.unwrap();

    // Assert
    assert_eq!(stop, StopCondition::BudgetExhausted);
    assert_eq!(fixture.calls(), 0);
}

#[tokio::test]
async fn the_turn_that_reaches_the_limit_completes_and_the_next_is_refused() {
    // Arrange
    let fixture = Fixture::new();
    let (mut lp, _events) = fixture.turn_loop(
        vec![end_turn(usage(6, 4)), end_turn(usage(1, 1))],
        ToolSet::new(),
        LIMIT,
    );

    // Act
    let first = lp.send("one").await.unwrap();
    let second = lp.send("two").await.unwrap();

    // Assert
    assert_eq!(first, StopCondition::EndTurn);
    assert_eq!(second, StopCondition::BudgetExhausted);
    assert_eq!(fixture.calls(), 1);
    assert_valid_history(lp.conversation().messages());
}

#[tokio::test]
async fn one_token_below_the_limit_still_admits_the_next_call() {
    // Arrange
    let fixture = Fixture::new();
    let (mut lp, _events) = fixture.turn_loop(
        vec![end_turn(usage(5, 4)), end_turn(usage(1, 1))],
        ToolSet::new(),
        LIMIT,
    );

    // Act
    let first = lp.send("one").await.unwrap();
    let second = lp.send("two").await.unwrap();

    // Assert
    assert_eq!(first, StopCondition::EndTurn);
    assert_eq!(second, StopCondition::EndTurn);
    assert_eq!(fixture.calls(), 2);
}

#[tokio::test]
async fn a_limit_reached_mid_tool_loop_stops_after_the_current_call() {
    // Arrange
    let fixture = Fixture::new();
    let two_calls = tool_turn(
        &[("c1", "canned", "{}"), ("c2", "canned", "{}")],
        usage(6, 4),
    );
    let (mut lp, _events) = fixture.turn_loop(
        vec![two_calls, end_turn(usage(1, 1))],
        ToolSet::new().with(Arc::new(Canned)),
        LIMIT,
    );

    // Act
    let stop = lp.send("go").await.unwrap();

    // Assert
    assert_eq!(stop, StopCondition::BudgetExhausted);
    assert_eq!(fixture.calls(), 1);
    let results: Vec<_> = lp
        .conversation()
        .messages()
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult {
                call_id,
                text,
                is_error,
            } => Some((call_id.as_str(), text.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results[0], ("c1", "ran", false));
    assert_eq!(results[1].0, "c2");
    assert!(results[1].2, "the skipped call is reported as an error");
    assert_valid_history(lp.conversation().messages());
}

#[tokio::test]
async fn context_overflow_is_a_terminal_stop_with_a_user_visible_event() {
    // Arrange
    let fixture = Fixture::new();
    let (mut lp, mut events) = fixture.turn_loop(
        vec![failing(ProviderError::ContextWindowExceeded)],
        ToolSet::new(),
        1_000_000,
    );

    // Act
    let stop = lp.send("go").await.unwrap();

    // Assert
    assert_eq!(stop, StopCondition::ContextOverflow);
    assert_eq!(events.try_recv().unwrap(), TurnEvent::ContextOverflow);
    assert_eq!(fixture.calls(), 1, "the loop does not retry or compact");
    assert_valid_history(lp.conversation().messages());
}
