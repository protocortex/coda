// SPDX-License-Identifier: Apache-2.0

//! Read-only tool calls from one reply run at the same time; every other call
//! runs alone, in order. Results are always stored in the order of the calls.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coda_core::prompt::{Conversation, Prefix};
use coda_core::turn::{CancelToken, StopCondition, ToolSet, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{ModelEvent, StopReason};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store};
use coda_tools::{Tool, ToolError, ToolOutput};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

const READ: &str = "probe";
const WRITE: &str = "mutate";
const MEET_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(1);
const WORK_MS: u64 = 30;
const PROMPT_MS: u64 = 5;

/// What the probes saw of each other.
#[derive(Default)]
struct Shared {
    active: AtomicUsize,
    max_active: AtomicUsize,
    arrived: AtomicUsize,
    writing: AtomicBool,
    write_saw_active: AtomicUsize,
    read_saw_write: AtomicBool,
    ran: AtomicUsize,
}

struct Probe {
    name: &'static str,
    read_only: bool,
    shared: Arc<Shared>,
    cancel: CancelToken,
}

impl Probe {
    fn wait_for_peers(&self, meet: usize) -> Result<(), ToolError> {
        self.shared.arrived.fetch_add(1, Ordering::SeqCst);
        let start = Instant::now();
        while self.shared.arrived.load(Ordering::SeqCst) < meet {
            if start.elapsed() > MEET_TIMEOUT {
                return Err(ToolError::InvalidInput {
                    tool: self.name,
                    reason: "no other call ran at the same time".to_owned(),
                });
            }
            std::thread::sleep(POLL);
        }
        Ok(())
    }
}

impl Tool for Probe {
    fn name(&self) -> &'static str {
        self.name
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let label = input["label"].as_str().unwrap_or_default().to_owned();
        let ms = input["ms"].as_u64().unwrap_or(WORK_MS);
        let meet = usize::try_from(input["meet"].as_u64().unwrap_or(0)).unwrap();
        let shared = &self.shared;
        shared.ran.fetch_add(1, Ordering::SeqCst);
        if self.read_only {
            if shared.writing.load(Ordering::SeqCst) {
                shared.read_saw_write.store(true, Ordering::SeqCst);
            }
        } else {
            shared
                .write_saw_active
                .store(shared.active.load(Ordering::SeqCst), Ordering::SeqCst);
            shared.writing.store(true, Ordering::SeqCst);
        }
        let now = shared.active.fetch_add(1, Ordering::SeqCst) + 1;
        shared.max_active.fetch_max(now, Ordering::SeqCst);
        let rendezvous = if meet > 0 {
            self.wait_for_peers(meet)
        } else {
            Ok(())
        };
        std::thread::sleep(Duration::from_millis(ms));
        if input["cancel"].as_bool().unwrap_or(false) {
            self.cancel.cancel();
        }
        shared.writing.store(false, Ordering::SeqCst);
        shared.active.fetch_sub(1, Ordering::SeqCst);
        rendezvous?;
        Ok(ToolOutput {
            text: label,
            truncated: false,
        })
    }
}

/// Allows everything and records how many prompts were open at once.
#[derive(Default)]
struct Prompts {
    open: AtomicUsize,
    max_open: AtomicUsize,
    asked: AtomicUsize,
    cancel_on_first: Mutex<Option<CancelToken>>,
}

#[derive(Clone)]
struct Gate(Arc<Prompts>);

impl PermissionGate for Gate {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        let prompts = Arc::clone(&self.0);
        async move {
            let now = prompts.open.fetch_add(1, Ordering::SeqCst) + 1;
            prompts.max_open.fetch_max(now, Ordering::SeqCst);
            let first = prompts.asked.fetch_add(1, Ordering::SeqCst) == 0;
            if first && let Some(token) = prompts.cancel_on_first.lock().unwrap().as_ref() {
                token.cancel();
            }
            tokio::time::sleep(Duration::from_millis(PROMPT_MS)).await;
            prompts.open.fetch_sub(1, Ordering::SeqCst);
            PermissionDecision::AllowOnce
        }
    }
}

fn calls(specs: &[(&str, Value)]) -> Turn {
    let mut events = Vec::new();
    for (index, (name, arguments)) in specs.iter().enumerate() {
        let call_id = format!("c{index}");
        events.push(ModelEvent::ToolCallStart {
            call_id: call_id.clone(),
            name: (*name).to_owned(),
        });
        events.push(ModelEvent::ToolCallArgumentsDelta {
            call_id,
            fragment: arguments.to_string(),
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
    lp: TurnLoop<FauxProvider, Gate>,
    events: UnboundedReceiver<TurnEvent>,
    shared: Arc<Shared>,
    prompts: Arc<Prompts>,
    _dir: tempfile::TempDir,
}

fn rig(first: Turn, limit: Option<usize>) -> Rig {
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
    let shared = Arc::new(Shared::default());
    let prompts = Arc::new(Prompts::default());
    let cancel = CancelToken::default();
    let probe = |name, read_only| -> Arc<dyn Tool> {
        Arc::new(Probe {
            name,
            read_only,
            shared: Arc::clone(&shared),
            cancel: cancel.clone(),
        })
    };
    let tools = ToolSet::new()
        .with(probe(READ, true))
        .with(probe(WRITE, false));
    let mut lp = TurnLoop::new(
        FauxProvider::new(vec![first, done()]),
        Gate(Arc::clone(&prompts)),
        tools,
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
    )
    .with_cancel_token(cancel);
    if let Some(limit) = limit {
        lp = lp.with_max_parallel_tools(limit);
    }
    Rig {
        lp,
        events,
        shared,
        prompts,
        _dir: dir,
    }
}

fn results(events: &mut UnboundedReceiver<TurnEvent>) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let TurnEvent::ToolFinished { text, is_error, .. } = event {
            out.push((text, is_error));
        }
    }
    out
}

fn texts(events: &mut UnboundedReceiver<TurnEvent>) -> Vec<String> {
    results(events).into_iter().map(|(text, _)| text).collect()
}

fn read(label: &str) -> (&'static str, Value) {
    (READ, json!({"label": label}))
}

#[tokio::test]
async fn three_read_only_calls_run_at_the_same_time() {
    let specs: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|label| (READ, json!({"label": label, "meet": 3})))
        .collect();
    let mut rig = rig(calls(&specs), None);

    let stop = rig.lp.send("look").await.unwrap();

    assert_eq!(stop, StopCondition::EndTurn);
    assert_eq!(
        results(&mut rig.events),
        [
            ("a".to_owned(), false),
            ("b".to_owned(), false),
            ("c".to_owned(), false)
        ]
    );
    assert_eq!(rig.shared.max_active.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_changing_call_between_reads_never_overlaps_another_call() {
    let mut rig = rig(
        calls(&[
            (READ, json!({"label": "a", "meet": 2})),
            (READ, json!({"label": "b", "meet": 2})),
            (WRITE, json!({"label": "w"})),
            read("c"),
            read("d"),
        ]),
        None,
    );

    rig.lp.send("edit").await.unwrap();

    assert_eq!(texts(&mut rig.events), ["a", "b", "w", "c", "d"]);
    assert_eq!(rig.shared.write_saw_active.load(Ordering::SeqCst), 0);
    assert!(!rig.shared.read_saw_write.load(Ordering::SeqCst));
    assert_eq!(rig.shared.max_active.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn results_follow_the_order_of_the_calls_when_the_first_is_slowest() {
    let specs: Vec<_> = [("a", 90_u64), ("b", 5), ("c", 30)]
        .iter()
        .map(|(label, ms)| (READ, json!({"label": label, "ms": ms})))
        .collect();
    let mut rig = rig(calls(&specs), None);

    rig.lp.send("look").await.unwrap();

    assert_eq!(texts(&mut rig.events), ["a", "b", "c"]);
}

#[tokio::test]
async fn the_limit_caps_how_many_calls_run_at_once() {
    let specs: Vec<_> = (0..5)
        .map(|n| (READ, json!({"label": n.to_string(), "meet": 2})))
        .collect();
    let mut rig = rig(calls(&specs), Some(2));

    rig.lp.send("look").await.unwrap();

    assert_eq!(texts(&mut rig.events), ["0", "1", "2", "3", "4"]);
    assert_eq!(rig.shared.max_active.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_limit_of_one_runs_every_call_in_turn() {
    let specs: Vec<_> = (0..3).map(|n| read(&n.to_string())).collect();
    let mut rig = rig(calls(&specs), Some(1));

    rig.lp.send("look").await.unwrap();

    assert_eq!(texts(&mut rig.events), ["0", "1", "2"]);
    assert_eq!(rig.shared.max_active.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn permission_prompts_are_asked_one_at_a_time() {
    let specs: Vec<_> = (0..3).map(|n| read(&n.to_string())).collect();
    let mut rig = rig(calls(&specs), None);

    rig.lp.send("look").await.unwrap();

    assert_eq!(texts(&mut rig.events).len(), 3);
    assert_eq!(rig.prompts.max_open.load(Ordering::SeqCst), 1);
    assert_eq!(rig.prompts.asked.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_cancel_during_a_batch_still_answers_every_call() {
    let specs = [
        (READ, json!({"label": "a", "cancel": true})),
        read("b"),
        read("c"),
        (WRITE, json!({"label": "w"})),
    ];
    let mut rig = rig(calls(&specs), None);

    let stop = rig.lp.send("look").await.unwrap();

    assert_eq!(stop, StopCondition::Cancelled);
    let answers = results(&mut rig.events);
    assert_eq!(answers.len(), 4);
    assert!(
        answers[3].1,
        "the call after the batch is answered as skipped"
    );
}

#[tokio::test]
async fn a_cancel_while_the_first_prompt_is_open_runs_none_of_the_calls() {
    let specs: Vec<_> = (0..3).map(|n| read(&n.to_string())).collect();
    let mut rig = rig(calls(&specs), None);
    *rig.prompts.cancel_on_first.lock().unwrap() = Some(rig.lp.cancel_token());

    let stop = rig.lp.send("look").await.unwrap();

    assert_eq!(stop, StopCondition::Cancelled);
    assert_eq!(results(&mut rig.events).len(), 3);
    assert_eq!(rig.shared.ran.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_call_that_ran_before_a_stop_keeps_its_real_result() {
    let specs = [
        (READ, json!({"label": "a", "cancel": true})),
        read("b"),
        read("c"),
    ];
    let mut rig = rig(calls(&specs), None);

    rig.lp.send("look").await.unwrap();

    assert_eq!(texts(&mut rig.events), ["a", "b", "c"]);
}
