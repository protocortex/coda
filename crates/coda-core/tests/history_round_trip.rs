// SPDX-License-Identifier: AGPL-3.0-only

//! The conversation a live loop holds equals the one rebuilt from the events
//! the daemon would have stored for it.

use std::future::Future;
use std::sync::{Arc, Mutex};

use coda_core::history::{
    ASSISTANT_MESSAGE_KIND, TOOL_FINISHED_KIND, USER_MESSAGE_KIND, assistant_body, empty_text_id,
    rebuild, tool_finished_body,
};
use coda_core::prompt::{Conversation, Message, Prefix};
use coda_core::turn::{StopCondition, ToolSet, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::types::{BackendKind, Mode, OriginalId, PermissionDecision, SessionId, TurnId};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{ModelEvent, StopReason};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store, StoredEvent};
use coda_tools::{Tool, ToolError, ToolOutput};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;

const CANNED_OUTPUT: &str = "ran";

struct AllowAll;

impl PermissionGate for AllowAll {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::AllowOnce)
    }
}

struct Canned;

impl Tool for Canned {
    fn name(&self) -> &'static str {
        "canned"
    }

    fn call(&self, _input: &Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            text: CANNED_OUTPUT.to_owned(),
            truncated: false,
        })
    }
}

fn tool_turn(calls: &[(&str, &str)]) -> Turn {
    let mut events = Vec::new();
    for (id, arguments) in calls {
        events.push(ModelEvent::ToolCallStart {
            call_id: (*id).into(),
            name: "canned".into(),
        });
        events.push(ModelEvent::ToolCallArgumentsDelta {
            call_id: (*id).into(),
            fragment: (*arguments).into(),
        });
    }
    events.push(ModelEvent::Stop {
        reason: StopReason::ToolUse,
    });
    Turn {
        events,
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

struct Rig {
    _dir: TempDir,
    turn_loop: TurnLoop<FauxProvider, AllowAll>,
    events: UnboundedReceiver<TurnEvent>,
    stored: Vec<StoredEvent>,
    /// Tool results, kept where the daemon keeps them: apart from the events.
    originals: Vec<String>,
}

impl Rig {
    fn new(script: Vec<Turn>) -> Self {
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
        let (sender, events) = tokio::sync::mpsc::unbounded_channel();
        let turn_loop = TurnLoop::new(
            FauxProvider::new(script),
            AllowAll,
            ToolSet::new().with(Arc::new(Canned)),
            Arc::new(Mutex::new(store)),
            Conversation::new(prefix()),
            config(session),
            sender,
        );
        Self {
            _dir: dir,
            turn_loop,
            events,
            stored: Vec::new(),
            originals: Vec::new(),
        }
    }

    /// Sends `text` and stores the events the way the daemon does.
    async fn send(&mut self, text: &str) -> StopCondition {
        self.store(USER_MESSAGE_KIND, &json!({ "text": text }));
        let stop = self.turn_loop.send(text).await.unwrap();
        while let Ok(event) = self.events.try_recv() {
            match event {
                TurnEvent::Assistant { text, tool_calls } => {
                    let kept: Vec<_> = tool_calls
                        .iter()
                        .map(|call| (call, Some(self.keep(call.arguments.clone()))))
                        .collect();
                    let body = assistant_body(&text, &kept);
                    self.store(ASSISTANT_MESSAGE_KIND, &body);
                }
                TurnEvent::ToolFinished {
                    tool,
                    call_id,
                    text,
                    is_error,
                } => {
                    let original = self.keep(text);
                    let body = tool_finished_body(&tool, &call_id, Some(&original), is_error);
                    self.store(TOOL_FINISHED_KIND, &body);
                }
                _ => {}
            }
        }
        stop
    }

    /// Keeps `text` apart from the events and returns the id it is kept under.
    fn keep(&mut self, text: String) -> OriginalId {
        if text.is_empty() {
            return empty_text_id();
        }
        self.originals.push(text);
        OriginalId::new((self.originals.len() - 1).to_string())
    }

    fn store(&mut self, kind: &str, body: &Value) {
        self.stored.push(StoredEvent {
            seq: self.stored.len() as u64 + 1,
            turn: TurnId::new("1"),
            kind: kind.to_owned(),
            body: body.to_string(),
            at: 0,
        });
    }

    fn replayed(&self) -> Vec<Message> {
        let lookup = |id: &OriginalId| {
            self.originals
                .get(id.as_str().parse::<usize>().ok()?)
                .cloned()
        };
        rebuild(prefix(), &self.stored, &lookup).messages().to_vec()
    }
}

fn prefix() -> Prefix {
    Prefix::new("You are Coda.", Vec::new(), "").unwrap()
}

fn config(session: SessionId) -> TurnConfig {
    TurnConfig {
        session,
        cwd: "/ws".into(),
        max_turns: 8,
        token_limit: 1_000_000,
        compact_threshold_bytes: 1024,
    }
}

#[tokio::test]
async fn a_replayed_tool_session_equals_the_conversation_the_live_loop_held() {
    // Arrange
    let mut rig = Rig::new(vec![
        tool_turn(&[("c1", "{}")]),
        tool_turn(&[("c2", "{}"), ("c3", "{\"x\":1}")]),
        text_turn("all done"),
        text_turn("second answer"),
    ]);

    // Act
    rig.send("first question").await;
    rig.send("follow up").await;

    // Assert
    let live = rig.turn_loop.conversation().messages().to_vec();
    assert_eq!(live.len(), 9, "{live:?}");
    assert_eq!(rig.replayed(), live);
}

#[tokio::test]
async fn a_replayed_cancelled_send_equals_the_conversation_the_live_loop_held() {
    // Arrange
    let mut rig = Rig::new(vec![text_turn("never streamed")]);
    rig.turn_loop.cancel_token().cancel();

    // Act
    let stop = rig.send("cancel me").await;

    // Assert
    assert_eq!(stop, StopCondition::Cancelled);
    let live = rig.turn_loop.conversation().messages().to_vec();
    assert_eq!(live.len(), 2, "{live:?}");
    assert_eq!(rig.replayed(), live);
}
