// SPDX-License-Identifier: Apache-2.0

//! The `todo` tool in the turn loop: the model keeps a short list, each change
//! is an event, and a loop with no list refuses the tool.

use std::future::Future;
use std::sync::{Arc, Mutex};

use coda_core::prompt::{Conversation, Prefix};
use coda_core::turn::{StopCondition, ToolSet, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::events::{TodoItem, TodoStatus};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{ModelEvent, StopReason};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

struct AllowAll;

impl PermissionGate for AllowAll {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::AllowOnce)
    }
}

fn call(arguments: &Value) -> Turn {
    Turn {
        events: vec![
            ModelEvent::ToolCallStart {
                call_id: "c1".into(),
                name: "todo".into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                call_id: "c1".into(),
                fragment: arguments.to_string(),
            },
            ModelEvent::Stop {
                reason: StopReason::ToolUse,
            },
        ],
        error: None,
    }
}

fn done() -> Turn {
    Turn {
        events: vec![
            ModelEvent::TextDelta {
                text: "done".into(),
            },
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

fn rig(turns: Vec<Turn>, todos: Option<Vec<TodoItem>>) -> Rig {
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
    let mut lp = TurnLoop::new(
        FauxProvider::new(turns),
        AllowAll,
        ToolSet::new(),
        Arc::new(Mutex::new(store)),
        Conversation::new(Prefix::new("You are Coda.", Vec::new(), "").unwrap()),
        TurnConfig {
            session,
            cwd: dir.path().to_path_buf(),
            max_turns: 8,
            token_limit: 1_000_000,
            compact_threshold_bytes: 1024 * 1024,
        },
        sender,
    );
    if let Some(items) = todos {
        lp = lp.with_todos(items);
    }
    Rig {
        lp,
        events,
        _dir: dir,
    }
}

/// The text of each tool result and each list an update carried.
fn seen(events: &mut UnboundedReceiver<TurnEvent>) -> (Vec<(String, bool)>, Vec<Vec<TodoItem>>) {
    let (mut results, mut lists) = (Vec::new(), Vec::new());
    while let Ok(event) = events.try_recv() {
        match event {
            TurnEvent::ToolFinished { text, is_error, .. } => results.push((text, is_error)),
            TurnEvent::TodoUpdated { items } => lists.push(items),
            _ => {}
        }
    }
    (results, lists)
}

fn item(id: u32, text: &str, status: TodoStatus) -> TodoItem {
    TodoItem {
        id,
        text: text.to_owned(),
        status,
    }
}

#[tokio::test]
async fn an_added_item_is_answered_with_the_list_and_announced_as_an_update() {
    let mut rig = rig(
        vec![
            call(&json!({"action": "add", "text": "write tests"})),
            done(),
        ],
        Some(Vec::new()),
    );

    let stop = rig.lp.send("plan").await.unwrap();

    assert_eq!(stop, StopCondition::EndTurn);
    let (results, lists) = seen(&mut rig.events);
    assert_eq!(results, [("1. [ ] write tests".to_owned(), false)]);
    assert_eq!(lists, [vec![item(1, "write tests", TodoStatus::Pending)]]);
}

#[tokio::test]
async fn a_loop_continues_numbering_from_the_list_it_was_given() {
    let stored = vec![item(4, "old step", TodoStatus::Done)];
    let mut rig = rig(
        vec![call(&json!({"action": "add", "text": "next step"})), done()],
        Some(stored),
    );

    rig.lp.send("go on").await.unwrap();

    let (results, lists) = seen(&mut rig.events);
    assert_eq!(results[0].0, "4. [x] old step\n5. [ ] next step");
    assert_eq!(lists[0].len(), 2);
}

#[tokio::test]
async fn a_refused_change_is_an_error_the_model_can_read_and_sends_no_update() {
    let mut rig = rig(
        vec![
            call(&json!({"action": "set", "id": 7, "status": "done"})),
            done(),
        ],
        Some(Vec::new()),
    );

    rig.lp.send("finish").await.unwrap();

    let (results, lists) = seen(&mut rig.events);
    assert!(results[0].1, "{results:?}");
    assert!(results[0].0.contains("no item 7"), "{results:?}");
    assert_eq!(lists.len(), 0);
}

#[tokio::test]
async fn a_loop_with_no_list_refuses_the_tool_so_a_sub_agent_cannot_use_it() {
    let mut rig = rig(
        vec![call(&json!({"action": "add", "text": "x"})), done()],
        None,
    );

    rig.lp.send("plan").await.unwrap();

    let (results, lists) = seen(&mut rig.events);
    assert!(results[0].1, "{results:?}");
    assert!(results[0].0.contains("not available"), "{results:?}");
    assert_eq!(lists.len(), 0);
}
