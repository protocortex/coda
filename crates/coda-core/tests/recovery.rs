// SPDX-License-Identifier: Apache-2.0

//! Tool calls recovered from weak models: repaired arguments and, where the
//! provider opts in, a call written as the reply's text.

use std::future::Future;
use std::sync::{Arc, Mutex};

use coda_core::prompt::{Conversation, Message, Prefix, ToolSchema};
use coda_core::turn::{Recovery, StopCondition, ToolSet, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{ModelEvent, StopReason};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store};
use coda_tools::{Tool, ToolError, ToolOutput};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

const TOOL: &str = "record";

struct AllowAll;

impl PermissionGate for AllowAll {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::AllowOnce)
    }
}

/// A tool that keeps the arguments it was called with.
struct Record(Arc<Mutex<Vec<Value>>>);

impl Tool for Record {
    fn name(&self) -> &'static str {
        TOOL
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        self.0.lock().unwrap().push(input.clone());
        Ok(ToolOutput {
            text: "recorded".to_owned(),
            truncated: false,
        })
    }
}

type Loop = TurnLoop<FauxProvider, AllowAll>;

fn build(
    turns: Vec<Turn>,
    text_calls: bool,
) -> (
    Loop,
    UnboundedReceiver<TurnEvent>,
    Arc<Mutex<Vec<Value>>>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(&dir.path().join("data")).unwrap();
    let session = store
        .create_session(&NewSession {
            backend: BackendKind::Faux,
            model: None,
            cwd: "/ws".to_owned(),
            mode: Mode::default(),
        })
        .unwrap();
    std::fs::create_dir(dir.path().join("ws")).unwrap();
    let schema = ToolSchema {
        name: TOOL.into(),
        description: "Records a path.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        }),
    };
    let prefix = Prefix::new("You are Coda.", vec![schema], "").unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolSet::new().with(Arc::new(Record(Arc::clone(&seen))));
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let lp = TurnLoop::new(
        FauxProvider::new(turns),
        AllowAll,
        tools,
        Arc::new(Mutex::new(store)),
        Conversation::new(prefix),
        TurnConfig {
            session,
            cwd: dir.path().join("ws"),
            max_turns: 8,
            token_limit: 1_000_000,
            compact_threshold_bytes: 1024 * 1024,
        },
        sender,
    )
    .with_text_tool_calls(text_calls);
    (lp, receiver, seen, dir)
}

fn call_turn(arguments: &str) -> Turn {
    Turn {
        events: vec![
            ModelEvent::ToolCallStart {
                call_id: "c1".into(),
                name: TOOL.into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                call_id: "c1".into(),
                fragment: arguments.into(),
            },
            ModelEvent::Stop {
                reason: StopReason::ToolUse,
            },
        ],
        error: None,
    }
}

fn text_turn(text: &str) -> Turn {
    Turn {
        events: vec![
            ModelEvent::TextDelta { text: text.into() },
            ModelEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ],
        error: None,
    }
}

fn recoveries(events: &mut UnboundedReceiver<TurnEvent>) -> Vec<(String, Recovery)> {
    let mut found = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let TurnEvent::CallRecovered { tool, how, .. } = event {
            found.push((tool, how));
        }
    }
    found
}

fn tool_results(lp: &Loop) -> Vec<(String, bool)> {
    lp.conversation()
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult { text, is_error, .. } => Some((text.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn almost_valid_arguments_are_repaired_run_and_flagged() {
    // Arrange
    let (mut lp, mut events, seen, _dir) = build(
        vec![call_turn("{'path': 'a.txt',}"), text_turn("done")],
        false,
    );

    // Act
    let stop = lp.send("go").await.unwrap();

    // Assert
    assert_eq!(stop, StopCondition::EndTurn);
    assert_eq!(*seen.lock().unwrap(), [json!({"path": "a.txt"})]);
    assert_eq!(
        recoveries(&mut events),
        [(TOOL.to_owned(), Recovery::Repaired)]
    );
    let stored = lp
        .conversation()
        .messages()
        .iter()
        .find_map(|message| match message {
            Message::Assistant { tool_calls, .. } => {
                tool_calls.first().map(|call| call.arguments.clone())
            }
            _ => None,
        });
    assert_eq!(stored.as_deref(), Some(r#"{"path": "a.txt"}"#));
}

#[tokio::test]
async fn arguments_that_cannot_be_repaired_are_refused_with_the_schema_and_never_run() {
    // Arrange
    let (mut lp, mut events, seen, _dir) = build(
        vec![call_turn("{path: 'a.txt'}"), text_turn("sorry")],
        false,
    );

    // Act
    lp.send("go").await.unwrap();

    // Assert
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(recoveries(&mut events).len(), 0);
    let results = tool_results(&lp);
    assert!(results[0].1);
    assert!(results[0].0.contains("not valid JSON"), "{results:?}");
    assert!(
        results[0].0.contains(r#""required":["path"]"#),
        "{results:?}"
    );
}

#[tokio::test]
async fn a_text_shaped_call_is_ignored_unless_the_provider_opts_in() {
    // Arrange
    let (mut lp, mut events, seen, _dir) = build(vec![text_turn(r#"{"path": "a.txt"}"#)], false);

    // Act
    let stop = lp.send("go").await.unwrap();

    // Assert
    assert_eq!(stop, StopCondition::EndTurn);
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(recoveries(&mut events).len(), 0);
}

#[tokio::test]
async fn a_text_shaped_call_that_fits_one_tool_runs_when_the_provider_opts_in() {
    // Arrange
    let (mut lp, mut events, seen, _dir) = build(
        vec![text_turn(r#"{"path": "a.txt"}"#), text_turn("done")],
        true,
    );

    // Act
    let stop = lp.send("go").await.unwrap();

    // Assert
    assert_eq!(stop, StopCondition::EndTurn);
    assert_eq!(*seen.lock().unwrap(), [json!({"path": "a.txt"})]);
    assert_eq!(
        recoveries(&mut events),
        [(TOOL.to_owned(), Recovery::FromText)]
    );
    assert_eq!(tool_results(&lp), [("recorded".to_owned(), false)]);
}

#[tokio::test]
async fn a_reply_naming_an_unregistered_tool_or_not_fitting_is_never_run() {
    for text in [
        r#"{"name": "rm", "arguments": {"path": "/"}}"#,
        r#"{"path": "a.txt", "extra": 1}"#,
        r#"Here you go: {"path": "a.txt"}"#,
    ] {
        // Arrange
        let (mut lp, mut events, seen, _dir) = build(vec![text_turn(text)], true);

        // Act
        let stop = lp.send("go").await.unwrap();

        // Assert
        assert_eq!(stop, StopCondition::EndTurn, "{text}");
        assert!(seen.lock().unwrap().is_empty(), "{text}");
        assert_eq!(recoveries(&mut events).len(), 0, "{text}");
    }
}
