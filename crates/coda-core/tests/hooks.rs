// SPDX-License-Identifier: AGPL-3.0-only

//! Hooks in the turn loop: what they can stop and what the model and the user
//! are told. The runner itself is tested in `hook_runner.rs`.

mod common;

use std::ffi::OsString;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use coda_config::hooks::{HookConfig, HookEvent};
use coda_core::hooks::HookRunner;
use coda_core::prompt::{Conversation, Message, Prefix, ToolSchema};
use coda_core::turn::{StopCondition, ToolSet, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision, SessionId};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{ModelEvent, StopReason};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store};
use coda_tools::{Tool, ToolError, ToolOutput};
use common::{BOUND, Scratch, hook, pid_from, poll_until, process_alive, shell};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

const TOOL: &str = "canned";

struct AllowAll;

impl PermissionGate for AllowAll {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::AllowOnce)
    }
}

/// A tool that counts its calls and answers with a fixed text.
struct Canned {
    calls: Arc<AtomicUsize>,
    answer: String,
}

impl Tool for Canned {
    fn name(&self) -> &'static str {
        TOOL
    }

    fn call(&self, _input: &Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            text: self.answer.clone(),
            truncated: false,
        })
    }
}

type Loop = TurnLoop<FauxProvider, AllowAll>;

struct Fixture {
    scratch: Scratch,
    store: Arc<Mutex<Store>>,
    session: SessionId,
    calls: Arc<AtomicUsize>,
}

impl Fixture {
    fn new() -> Self {
        let scratch = Scratch::new();
        let mut store = Store::open(&scratch.out("data")).unwrap();
        let session = store
            .create_session(&NewSession {
                backend: BackendKind::Faux,
                model: None,
                cwd: "/ws".to_owned(),
                mode: Mode::default(),
            })
            .unwrap();
        std::fs::create_dir(scratch.out("ws")).unwrap();
        Self {
            scratch,
            store: Arc::new(Mutex::new(store)),
            session,
            calls: Arc::default(),
        }
    }

    /// A loop for a new conversation, whose tool answers `answer`.
    fn turn_loop(
        &self,
        turns: Vec<Turn>,
        hooks: &[HookConfig],
        answer: &str,
    ) -> (Loop, UnboundedReceiver<TurnEvent>) {
        self.build(turns, hooks, answer, false)
    }

    /// The same, for a conversation that already has messages.
    fn resumed_loop(&self, hooks: &[HookConfig]) -> (Loop, UnboundedReceiver<TurnEvent>) {
        self.build(vec![end_turn()], hooks, "ran", true)
    }

    fn build(
        &self,
        turns: Vec<Turn>,
        hooks: &[HookConfig],
        answer: &str,
        earlier_messages: bool,
    ) -> (Loop, UnboundedReceiver<TurnEvent>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let schema = ToolSchema {
            name: TOOL.into(),
            description: "Does a thing.".into(),
            input_schema: json!({"type": "object"}),
        };
        let prefix = Prefix::new("You are Coda.", vec![schema], "").unwrap();
        let mut conversation = Conversation::new(prefix);
        if earlier_messages {
            conversation.push(Message::User("earlier".to_owned()));
            conversation.push(Message::Assistant {
                text: "before".to_owned(),
                tool_calls: Vec::new(),
            });
        }
        let tools = ToolSet::new().with(Arc::new(Canned {
            calls: Arc::clone(&self.calls),
            answer: answer.to_owned(),
        }));
        let config = TurnConfig {
            session: self.session.clone(),
            cwd: self.scratch.out("ws"),
            max_turns: 8,
            token_limit: 1_000_000,
            compact_threshold_bytes: 1024 * 1024,
        };
        let env = vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))];
        let lp = TurnLoop::new(
            FauxProvider::new(turns),
            AllowAll,
            tools,
            Arc::clone(&self.store),
            conversation,
            config,
            sender,
        )
        .with_hooks(HookRunner::new(hooks, env));
        (lp, receiver)
    }

    fn tool_ran(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn end_turn() -> Turn {
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

fn tool_turn() -> Turn {
    Turn {
        events: vec![
            ModelEvent::ToolCallStart {
                call_id: "c1".into(),
                name: TOOL.into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                call_id: "c1".into(),
                fragment: json!({"path": "a.txt"}).to_string(),
            },
            ModelEvent::Stop {
                reason: StopReason::ToolUse,
            },
        ],
        error: None,
    }
}

/// A turn that calls the tool and, after its result, ends.
fn one_call() -> Vec<Turn> {
    vec![tool_turn(), end_turn()]
}

/// The tool results the conversation holds, as text and error flag.
fn results(lp: &Loop) -> Vec<(String, bool)> {
    lp.conversation()
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult { text, is_error, .. } => Some((text.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

async fn send(lp: &mut Loop, text: &str) -> StopCondition {
    tokio::time::timeout(BOUND, lp.send(text))
        .await
        .expect("the send did not finish in time")
        .unwrap()
}

#[tokio::test]
async fn a_failing_blocking_pre_tool_hook_stops_the_tool_and_the_model_is_told_why() {
    // Arrange
    let f = Fixture::new();
    let hook_path = f
        .scratch
        .script("deny.sh", "echo 'no deploys on friday' >&2\nexit 3");
    let hooks = [hook(HookEvent::PreTool, &[&hook_path], true)];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, "ran");

    // Act
    let stop = send(&mut lp, "go").await;

    // Assert
    assert_eq!(stop, StopCondition::EndTurn);
    assert_eq!(f.tool_ran(), 0, "the tool must not run");
    let results = results(&lp);
    assert_eq!(results.len(), 1);
    assert!(results[0].1, "the model must see an error");
    assert!(results[0].0.contains("no deploys on friday"), "{results:?}");
    assert!(results[0].0.contains("pre_tool"), "{results:?}");
}

#[tokio::test]
async fn a_failing_blocking_post_tool_hook_replaces_the_result_with_an_error() {
    // Arrange
    let f = Fixture::new();
    let hook_path = f
        .scratch
        .script("scan.sh", "echo 'secret found' >&2\nexit 1");
    let hooks = [hook(HookEvent::PostTool, &[&hook_path], true)];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, "SECRET-OUTPUT");

    // Act
    let stop = send(&mut lp, "go").await;

    // Assert
    assert_eq!(stop, StopCondition::EndTurn);
    assert_eq!(f.tool_ran(), 1, "the tool ran before the hook");
    let results = results(&lp);
    assert!(results[0].1);
    assert!(results[0].0.contains("secret found"), "{results:?}");
    assert!(!results[0].0.contains("SECRET-OUTPUT"), "{results:?}");
}

#[tokio::test]
async fn a_failing_non_blocking_hook_does_not_stop_the_tool() {
    // Arrange
    let f = Fixture::new();
    let hooks = [
        shell(HookEvent::PreTool, "exit 7", false),
        shell(HookEvent::PostTool, "exit 7", false),
    ];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, "ran");

    // Act
    let stop = send(&mut lp, "go").await;

    // Assert
    assert_eq!(stop, StopCondition::EndTurn);
    assert_eq!(f.tool_ran(), 1);
    assert_eq!(results(&lp), [("ran".to_owned(), false)]);
}

#[tokio::test]
async fn a_blocking_session_start_hook_stops_the_turn_before_the_model_and_is_tried_again() {
    // Arrange
    let f = Fixture::new();
    let (flag, starts) = (f.scratch.out("allow"), f.scratch.out("starts.jsonl"));
    let script = format!(
        "cat >> {log}\necho >> {log}\ntest -e {flag} || {{ echo 'not on this network' >&2; exit 9; }}",
        log = starts.display(),
        flag = flag.display()
    );
    let hook_path = f.scratch.script("gate.sh", &script);
    let hooks = [hook(HookEvent::SessionStart, &[&hook_path], true)];
    // One scripted turn: if the blocked send called the model, the second would fail.
    let (mut lp, mut events) = f.turn_loop(vec![end_turn()], &hooks, "ran");

    // Act
    let first = send(&mut lp, "one").await;
    std::fs::write(&flag, "").unwrap();
    let second = send(&mut lp, "two").await;

    // Assert
    assert_eq!(first, StopCondition::HookBlocked);
    assert_eq!(first.as_str(), "hook_blocked");
    assert_eq!(second, StopCondition::EndTurn);
    let mut blocked = Vec::new();
    let mut answer = String::new();
    while let Ok(event) = events.try_recv() {
        match event {
            TurnEvent::HookBlocked { reason } => blocked.push(reason),
            TurnEvent::Text(text) => answer.push_str(&text),
            _ => {}
        }
    }
    assert_eq!(blocked.len(), 1);
    assert!(blocked[0].contains("not on this network"), "{blocked:?}");
    assert!(blocked[0].contains("session_start"), "{blocked:?}");
    // The reason is for the user: it is never answer text.
    assert_eq!(answer, "done");
    // A new conversation is not "resumed" because its first send was blocked.
    let payloads: Vec<Value> = std::fs::read_to_string(&starts)
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(payloads.len(), 2, "{payloads:?}");
    assert!(payloads.iter().all(|payload| payload["resumed"] == false));
}

#[tokio::test]
async fn a_session_start_hook_runs_once_per_loop_and_says_whether_the_conversation_had_history() {
    // Arrange
    let f = Fixture::new();
    let (fresh_log, resumed_log) = (f.scratch.out("fresh.json"), f.scratch.out("resumed.json"));
    let record = |file: &std::path::Path| {
        let script = format!("cat >> {0}\necho >> {0}", file.display());
        shell(HookEvent::SessionStart, &script, false)
    };
    let (mut fresh, _e1) = f.turn_loop(vec![end_turn(), end_turn()], &[record(&fresh_log)], "ran");
    let (mut resumed, _e2) = f.resumed_loop(&[record(&resumed_log)]);

    // Act
    send(&mut fresh, "one").await;
    send(&mut fresh, "two").await;
    send(&mut resumed, "one").await;

    // Assert
    let lines = |file: &std::path::Path| -> Vec<Value> {
        std::fs::read_to_string(file)
            .unwrap()
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    let (fresh_lines, resumed_lines) = (lines(&fresh_log), lines(&resumed_log));
    assert_eq!(fresh_lines.len(), 1, "{fresh_lines:?}");
    assert_eq!(fresh_lines[0]["event"], "session_start");
    assert_eq!(fresh_lines[0]["resumed"], false);
    assert_eq!(fresh_lines[0]["session"], f.session.as_str());
    assert_eq!(resumed_lines.len(), 1);
    assert_eq!(resumed_lines[0]["resumed"], true);
}

#[tokio::test]
async fn cancelling_while_a_blocking_hook_runs_ends_the_turn_and_kills_the_hook() {
    // Arrange
    let f = Fixture::new();
    let pid_file = f.scratch.out("hook.pid");
    let script = format!("echo $$ > {}\nsleep 30", pid_file.display());
    let hooks = [shell(HookEvent::PreTool, &script, true)];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, "ran");
    let cancel = lp.cancel_token();

    let cancel_once_running = async {
        let _ = pid_from(&pid_file).await;
        cancel.cancel();
    };

    // Act
    let (stop, ()) = tokio::join!(send(&mut lp, "go"), cancel_once_running);

    // Assert
    assert_eq!(stop, StopCondition::Cancelled);
    assert_eq!(f.tool_ran(), 0);
    let hook_pid = pid_from(&pid_file).await;
    poll_until(|| !process_alive(&hook_pid)).await;
}
