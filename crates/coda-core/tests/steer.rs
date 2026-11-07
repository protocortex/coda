// SPDX-License-Identifier: Apache-2.0

//! A message steered into a running send joins the conversation at the next
//! step boundary, and keeps a send that was about to end going.

use std::future::Future;
use std::sync::{Arc, Mutex};

use coda_core::prompt::{Conversation, Message, Prefix};
use coda_core::turn::{StopCondition, ToolSet, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{ModelEvent, StopReason};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store};
use tokio::sync::mpsc::UnboundedReceiver;

struct AllowAll;

impl PermissionGate for AllowAll {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::AllowOnce)
    }
}

fn unknown_tool_call() -> Turn {
    Turn {
        events: vec![
            ModelEvent::ToolCallStart {
                call_id: "c1".into(),
                name: "nothing".into(),
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

fn answer(text: &str) -> Turn {
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
    lp: TurnLoop<FauxProvider, AllowAll>,
    events: UnboundedReceiver<TurnEvent>,
    _dir: tempfile::TempDir,
}

fn rig(turns: Vec<Turn>) -> Rig {
    rig_with_steps(turns, 8)
}

fn rig_with_steps(turns: Vec<Turn>, max_turns: u32) -> Rig {
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
    let lp = TurnLoop::new(
        FauxProvider::new(turns),
        AllowAll,
        ToolSet::new(),
        Arc::new(Mutex::new(store)),
        Conversation::new(Prefix::new("You are Coda.", Vec::new(), "").unwrap()),
        TurnConfig {
            session,
            cwd: dir.path().to_path_buf(),
            max_turns,
            token_limit: 1_000_000,
            compact_threshold_bytes: 1024 * 1024,
        },
        sender,
    );
    Rig {
        lp,
        events,
        _dir: dir,
    }
}

fn shape(lp: &TurnLoop<FauxProvider, AllowAll>) -> Vec<String> {
    lp.conversation()
        .messages()
        .iter()
        .map(|m| match m {
            Message::User(text) => format!("user:{text}"),
            Message::Assistant { text, .. } => format!("assistant:{text}"),
            Message::ToolResult { .. } => "tool_result".to_owned(),
            Message::Reasoning(_) => "reasoning".to_owned(),
        })
        .collect()
}

#[tokio::test]
async fn a_steer_joins_the_conversation_after_the_tool_results_of_the_step() {
    // Arrange
    let mut rig = rig(vec![unknown_tool_call(), answer("done")]);
    rig.lp.steer_queue().push("use tabs".to_owned());

    // Act
    let stop = rig.lp.send("write it").await.unwrap();

    // Assert
    assert_eq!(stop, StopCondition::EndTurn);
    assert_eq!(
        shape(&rig.lp),
        [
            "user:write it",
            "assistant:",
            "tool_result",
            "user:use tabs",
            "assistant:done"
        ]
    );
}

#[tokio::test]
async fn a_steer_that_arrives_as_the_send_would_end_keeps_it_going() {
    // Arrange
    let mut rig = rig(vec![answer("first"), answer("second")]);
    rig.lp.steer_queue().push("also this".to_owned());

    // Act
    let stop = rig.lp.send("go").await.unwrap();

    // Assert
    assert_eq!(stop, StopCondition::EndTurn);
    assert_eq!(
        shape(&rig.lp),
        [
            "user:go",
            "assistant:first",
            "user:also this",
            "assistant:second"
        ]
    );
}

#[tokio::test]
async fn a_steer_on_the_last_allowed_step_does_not_turn_a_finished_send_into_a_limit() {
    // Arrange
    let mut rig = rig_with_steps(vec![answer("only")], 1);
    rig.lp.steer_queue().push("too late".to_owned());

    // Act
    let stop = rig.lp.send("go").await.unwrap();

    // Assert
    assert_eq!(stop, StopCondition::EndTurn);
    assert!(!rig.lp.steer_queue().is_empty());
}

#[tokio::test]
async fn each_steered_message_is_reported_as_an_event() {
    // Arrange
    let mut rig = rig(vec![answer("first"), answer("second")]);
    rig.lp.steer_queue().push("also this".to_owned());

    // Act
    rig.lp.send("go").await.unwrap();

    // Assert
    let mut steered = Vec::new();
    while let Ok(event) = rig.events.try_recv() {
        if let TurnEvent::Steered(text) = event {
            steered.push(text);
        }
    }
    assert_eq!(steered, ["also this"]);
}

#[tokio::test]
async fn a_send_with_no_steer_is_unchanged() {
    // Arrange
    let mut rig = rig(vec![answer("only")]);

    // Act
    rig.lp.send("go").await.unwrap();

    // Assert
    assert_eq!(shape(&rig.lp), ["user:go", "assistant:only"]);
}
