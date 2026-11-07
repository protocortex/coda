// SPDX-License-Identifier: Apache-2.0

//! Reasoning blocks in the turn loop: kept out of the answer, sent back with the
//! next request, and absent when the model gave none.

use std::future::Future;
use std::sync::{Arc, Mutex};

use coda_core::prompt::{Conversation, Message, Prefix, ToolSchema};
use coda_core::turn::{StopCondition, ToolSet, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{
    ModelEvent, ModelProvider, ModelRequest, ModelStream, ProviderError, ReasoningBlock,
    RequestMessage, StopReason,
};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store};
use coda_tools::{Tool, ToolError, ToolOutput};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

const TOOL: &str = "echo";

struct AllowAll;

impl PermissionGate for AllowAll {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::AllowOnce)
    }
}

struct Echo;

impl Tool for Echo {
    fn name(&self) -> &'static str {
        TOOL
    }

    fn call(&self, _input: &Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            text: "echoed".to_owned(),
            truncated: false,
        })
    }
}

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

type Loop = TurnLoop<Recording, AllowAll>;

fn build(
    turns: Vec<Turn>,
) -> (
    Loop,
    UnboundedReceiver<TurnEvent>,
    Arc<Mutex<Vec<ModelRequest>>>,
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
        description: "Echoes.".into(),
        input_schema: json!({"type": "object"}),
    };
    let prefix = Prefix::new("You are Coda.", vec![schema], "").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let lp = TurnLoop::new(
        Recording {
            inner: FauxProvider::new(turns),
            requests: Arc::clone(&requests),
        },
        AllowAll,
        ToolSet::new().with(Arc::new(Echo)),
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
    );
    (lp, receiver, requests, dir)
}

fn thinking() -> ReasoningBlock {
    ReasoningBlock::Thinking {
        text: "I should call echo".to_owned(),
        signature: Some("sig-1".to_owned()),
    }
}

fn reasoning_call_turn() -> Turn {
    Turn {
        events: vec![
            ModelEvent::Reasoning { block: thinking() },
            ModelEvent::ToolCallStart {
                call_id: "c1".into(),
                name: TOOL.into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                call_id: "c1".into(),
                fragment: "{}".into(),
            },
            ModelEvent::Stop {
                reason: StopReason::ToolUse,
            },
        ],
        error: None,
    }
}

fn answer_turn() -> Turn {
    Turn {
        events: vec![
            ModelEvent::TextDelta {
                text: "the answer".into(),
            },
            ModelEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ],
        error: None,
    }
}

#[tokio::test]
async fn reasoning_stays_out_of_the_answer_and_goes_back_unchanged_on_the_next_request() {
    // Arrange
    let (mut lp, mut events, requests, _dir) = build(vec![reasoning_call_turn(), answer_turn()]);

    // Act
    let stop = lp.send("go").await.unwrap();

    // Assert
    assert_eq!(stop, StopCondition::EndTurn);
    let mut shown = String::new();
    let mut assistant_reasoning = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            TurnEvent::Text(text) => shown.push_str(&text),
            TurnEvent::Assistant { reasoning, .. } => assistant_reasoning.push(reasoning),
            _ => {}
        }
    }
    assert_eq!(shown, "the answer");
    assert_eq!(assistant_reasoning, [vec![thinking()], Vec::new()]);
    let requests = requests.lock().unwrap();
    let second = &requests[1].messages;
    assert!(matches!(&second[0], RequestMessage::User(_)));
    assert_eq!(second[1], RequestMessage::Reasoning(vec![thinking()]));
    assert!(
        matches!(&second[2], RequestMessage::Assistant { tool_calls, .. } if tool_calls.len() == 1)
    );
    assert!(matches!(&second[3], RequestMessage::ToolResult { .. }));
}

#[tokio::test]
async fn a_session_without_reasoning_sends_no_reasoning_message() {
    // Arrange
    let call = Turn {
        events: reasoning_call_turn()
            .events
            .into_iter()
            .filter(|event| !matches!(event, ModelEvent::Reasoning { .. }))
            .collect(),
        error: None,
    };
    let (mut lp, _events, requests, _dir) = build(vec![call, answer_turn()]);

    // Act
    lp.send("go").await.unwrap();

    // Assert
    let requests = requests.lock().unwrap();
    assert!(
        requests[1]
            .messages
            .iter()
            .all(|message| !matches!(message, RequestMessage::Reasoning(_)))
    );
    assert!(
        lp.conversation()
            .messages()
            .iter()
            .all(|message| !matches!(message, Message::Reasoning(_)))
    );
}
