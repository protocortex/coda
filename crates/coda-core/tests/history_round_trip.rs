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
use coda_protocol::events::{COMPACTION_KIND, Event};
use coda_protocol::types::{BackendKind, Mode, OriginalId, PermissionDecision, SessionId, TurnId};
use coda_providers::anthropic::request::{RequestSettings, build_body as anthropic_body};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{
    ModelEvent, ModelProvider, ModelRequest, ModelStream, ProviderError, StopReason,
};
use coda_providers::openai_compat::request::build_body as openai_body;
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
        let mut seen = Vec::new();
        let stop = {
            let send = self.turn_loop.send(text);
            tokio::pin!(send);
            loop {
                tokio::select! {
                    stop = &mut send => break stop.unwrap(),
                    Some(event) = self.events.recv() => {
                        // The daemon stores a compaction before the loop goes on.
                        if let TurnEvent::Compaction { ack, .. } = &event {
                            ack.confirm(true);
                        }
                        seen.push(event);
                    }
                }
            }
        };
        while let Ok(event) = self.events.try_recv() {
            seen.push(event);
        }
        for event in seen {
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
                TurnEvent::Compaction {
                    summary,
                    replaced,
                    kept,
                    ..
                } => {
                    let summary = self.keep(summary);
                    let event = Event::Compaction {
                        session_id: SessionId::new("s"),
                        turn_id: TurnId::new("1"),
                        summary,
                        replaced_messages: replaced.try_into().unwrap(),
                        kept_messages: kept.try_into().unwrap(),
                    };
                    self.store(COMPACTION_KIND, &serde_json::to_value(event).unwrap());
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
async fn a_replayed_compaction_equals_the_conversation_the_live_loop_held() {
    // Arrange
    let overflow = Turn {
        events: Vec::new(),
        error: Some(ProviderError::ContextWindowExceeded),
    };
    let mut rig = Rig::new(vec![
        tool_turn(&[("c1", "{}"), ("c2", "{}")]),
        text_turn("first answer"),
        text_turn("second answer"),
        text_turn("third answer"),
        overflow,
        text_turn("the summary"),
        text_turn("fourth answer"),
        text_turn("fifth answer"),
    ]);
    for question in ["one", "two", "three"] {
        rig.send(question).await;
    }

    // Act
    rig.send("four").await;
    let right_after = rig.turn_loop.conversation().messages().to_vec();
    rig.send("five").await;

    // Assert
    assert!(matches!(&right_after[0], Message::User(text) if text.contains("the summary")));
    assert_eq!(right_after.len(), 8, "{right_after:?}");
    let live = rig.turn_loop.conversation().messages().to_vec();
    assert_eq!(rig.replayed(), live);
}

#[tokio::test]
async fn a_second_compaction_on_top_of_a_first_replays_to_the_live_conversation() {
    // Arrange
    let overflow = || Turn {
        events: Vec::new(),
        error: Some(ProviderError::ContextWindowExceeded),
    };
    let mut script = vec![tool_turn(&[("c1", "{}")])];
    script.extend(["a1", "a2", "a3"].map(text_turn));
    script.extend([overflow(), text_turn("first summary"), text_turn("a4")]);
    script.extend(["a5", "a6", "a7"].map(text_turn));
    script.extend([overflow(), text_turn("second summary"), text_turn("a8")]);
    let mut rig = Rig::new(script);
    for question in ["one", "two", "three", "four", "five", "six", "seven"] {
        rig.send(question).await;
    }

    // Act
    rig.send("eight").await;

    // Assert
    let live = rig.turn_loop.conversation().messages().to_vec();
    let summaries = live
        .iter()
        .filter(|m| matches!(m, Message::User(text) if text.contains("summary_of_earlier")))
        .count();
    assert_eq!(
        summaries, 1,
        "the second summary replaced the first: {live:?}"
    );
    assert!(matches!(&live[0], Message::User(text) if text.contains("second summary")));
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

/// Remembers the last request it was sent, then answers from the script.
struct Capturing {
    inner: FauxProvider,
    last: Arc<Mutex<ModelRequest>>,
}

impl ModelProvider for Capturing {
    fn stream(
        &self,
        request: ModelRequest,
    ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send {
        *self.last.lock().unwrap() = request.clone();
        self.inner.stream(request)
    }
}

#[tokio::test]
async fn a_replayed_history_is_valid_for_both_wire_formats() {
    // Arrange
    let mut rig = Rig::new(vec![tool_turn(&[("c1", "{}")]), text_turn("done")]);
    rig.send("use the tool").await;
    let last = Arc::default();
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
    let lookup = |id: &OriginalId| {
        rig.originals
            .get(id.as_str().parse::<usize>().ok()?)
            .cloned()
    };
    let replayed = rebuild(prefix(), &rig.stored, &lookup);
    let (sender, _events) = tokio::sync::mpsc::unbounded_channel();
    let mut resumed = TurnLoop::new(
        Capturing {
            inner: FauxProvider::new(vec![text_turn("again")]),
            last: Arc::clone(&last),
        },
        AllowAll,
        ToolSet::new(),
        Arc::new(Mutex::new(store)),
        replayed,
        config(session),
        sender,
    );

    // Act
    resumed.send("and once more").await.unwrap();

    // Assert
    let request = last.lock().unwrap().clone();
    let settings = RequestSettings {
        model: "m".to_owned(),
        max_tokens: 64,
        system: String::new(),
        tools: Vec::new(),
    };
    let anthropic = anthropic_body(&settings, &request);
    let openai = openai_body(&settings, &request);
    assert_eq!(
        anthropic["messages"][2]["content"][0]["type"],
        "tool_result"
    );
    assert_eq!(
        anthropic["messages"][2]["content"][0]["content"],
        CANNED_OUTPUT
    );
    assert_eq!(openai["messages"][3]["role"], "tool");
    assert_eq!(openai["messages"][3]["content"], CANNED_OUTPUT);
    assert_eq!(anthropic["messages"].as_array().unwrap().len(), 5);
    assert_eq!(openai["messages"].as_array().unwrap().len(), 6);
}
