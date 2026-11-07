// SPDX-License-Identifier: Apache-2.0

//! The `ask` tool in the turn loop: the question goes to the gate, and every way
//! the answer can end is told to the model in words.

use std::future::Future;
use std::sync::{Arc, Mutex};

use coda_core::prompt::{Conversation, Prefix};
use coda_core::turn::{StopCondition, ToolSet, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::{AskAnswer, PermissionGate};
use coda_providers::model::{ModelEvent, StopReason};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

/// A question and the options it offered.
type Asked = (String, Vec<String>);

/// A gate that answers every question the same way and remembers what it was asked.
struct Answering {
    answer: AskAnswer,
    asked: Arc<Mutex<Vec<Asked>>>,
}

impl PermissionGate for Answering {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::AllowOnce)
    }

    fn ask(&self, question: &str, options: &[String]) -> impl Future<Output = AskAnswer> + Send {
        self.asked
            .lock()
            .unwrap()
            .push((question.to_owned(), options.to_vec()));
        std::future::ready(self.answer)
    }
}

/// A gate that does not override `ask`, as a backend with no way to ask.
struct Silent;

impl PermissionGate for Silent {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::AllowOnce)
    }
}

fn call(arguments: &Value) -> Turn {
    Turn {
        events: vec![
            ModelEvent::ToolCallStart {
                call_id: "c1".into(),
                name: "ask".into(),
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

fn question() -> Value {
    json!({"question": "Which file?", "options": ["a.rs", "b.rs"]})
}

struct Rig<G: PermissionGate> {
    lp: TurnLoop<FauxProvider, G>,
    events: UnboundedReceiver<TurnEvent>,
    _dir: tempfile::TempDir,
}

fn rig<G: PermissionGate>(gate: G, turns: Vec<Turn>, asks: bool) -> Rig<G> {
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
        gate,
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
    if asks {
        lp = lp.with_ask();
    }
    Rig {
        lp,
        events,
        _dir: dir,
    }
}

fn result(events: &mut UnboundedReceiver<TurnEvent>) -> (String, bool) {
    let mut found = None;
    while let Ok(event) = events.try_recv() {
        if let TurnEvent::ToolFinished { text, is_error, .. } = event {
            found = Some((text, is_error));
        }
    }
    found.expect("the call did not finish")
}

async fn asked(answer: AskAnswer) -> ((String, bool), Vec<Asked>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let gate = Answering {
        answer,
        asked: Arc::clone(&log),
    };
    let mut rig = rig(gate, vec![call(&question()), done()], true);
    let stop = rig.lp.send("go").await.unwrap();
    assert_eq!(stop, StopCondition::EndTurn);
    let seen = log.lock().unwrap().clone();
    (result(&mut rig.events), seen)
}

#[tokio::test]
async fn the_option_the_user_picked_is_named_in_the_result() {
    let (outcome, seen) = asked(AskAnswer::Chose(1)).await;

    assert_eq!(outcome, ("The user chose option 2: b.rs".to_owned(), false));
    assert_eq!(
        seen,
        [(
            "Which file?".to_owned(),
            vec!["a.rs".to_owned(), "b.rs".to_owned()]
        )]
    );
}

#[tokio::test]
async fn a_declined_question_and_a_timeout_are_clear_notes_and_not_errors() {
    let (declined, _) = asked(AskAnswer::Declined).await;
    let (timed_out, _) = asked(AskAnswer::TimedOut).await;

    assert_eq!(
        declined,
        ("The user did not choose an option.".to_owned(), false)
    );
    assert_eq!(
        timed_out,
        ("The user did not answer in time.".to_owned(), false)
    );
}

#[tokio::test]
async fn no_terminal_and_a_gate_that_cannot_ask_are_errors_the_model_can_read() {
    let (headless, _) = asked(AskAnswer::NoTerminal).await;
    let mut silent = rig(Silent, vec![call(&question()), done()], true);
    silent.lp.send("go").await.unwrap();
    let unavailable = result(&mut silent.events);

    for (text, is_error) in [headless, unavailable] {
        assert!(is_error, "{text}");
        assert!(text.contains("no one is at a terminal"), "{text}");
    }
}

#[tokio::test]
async fn a_loop_that_may_not_ask_refuses_the_tool_and_never_asks() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let gate = Answering {
        answer: AskAnswer::Chose(0),
        asked: Arc::clone(&log),
    };
    let mut rig = rig(gate, vec![call(&question()), done()], false);

    rig.lp.send("go").await.unwrap();

    let (text, is_error) = result(&mut rig.events);
    assert!(is_error && text.contains("sub-agents cannot ask"), "{text}");
    assert_eq!(log.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn bad_arguments_are_refused_before_anyone_is_asked() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let gate = Answering {
        answer: AskAnswer::Chose(0),
        asked: Arc::clone(&log),
    };
    let one_option = json!({"question": "q", "options": ["only"]});
    let mut rig = rig(gate, vec![call(&one_option), done()], true);

    rig.lp.send("go").await.unwrap();

    let (text, is_error) = result(&mut rig.events);
    assert!(is_error && text.contains("options"), "{text}");
    assert_eq!(log.lock().unwrap().len(), 0);
}
