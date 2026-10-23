// SPDX-License-Identifier: AGPL-3.0-only

//! Hooks in the turn loop: what they can stop and what the model and the user
//! are told. The runner itself is tested in `hook_runner.rs`.

mod common;

use std::ffi::OsString;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coda_config::hooks::{HookConfig, HookEvent};
use coda_core::hooks::HookRunner;
use coda_core::prompt::{Conversation, Message, Prefix, ToolSchema};
use coda_core::tools::StoreOriginals;
use coda_core::turn::{CancelToken, StopCondition, ToolSet, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::readers::ReaderError;
use coda_protocol::types::{BackendKind, Mode, PermissionDecision, SessionId};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{ModelEvent, StopReason};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store};
use coda_tools::retrieve::RetrieveTool;
use coda_tools::{Tool, ToolError, ToolOutput};
use common::{BOUND, Scratch, hook, pid_from, poll_until, process_alive, shell};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

const TOOL: &str = "canned";
/// A threshold no test output reaches, so the tool's answer is passed whole.
const NEVER_COMPACT: usize = 1024 * 1024;
/// How long a send may take to end once it is cancelled. A hook that is not
/// cut short by the cancel would keep it for its whole limit, which is longer.
const PROMPT: Duration = Duration::from_secs(5);

struct AllowAll;

impl PermissionGate for AllowAll {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::AllowOnce)
    }
}

/// Where a test hands the tool the token to cancel.
type CancelSlot = Arc<Mutex<Option<CancelToken>>>;

/// A tool that counts its calls and answers with a fixed text. It can also
/// cancel the turn as it answers, as a bash call does when it is interrupted,
/// or crash.
struct Canned {
    calls: Arc<AtomicUsize>,
    answer: String,
    cancel: CancelSlot,
    crashes: bool,
}

impl Tool for Canned {
    fn name(&self) -> &'static str {
        TOOL
    }

    fn call(&self, _input: &Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(!self.crashes, "the tool broke");
        if let Some(token) = self.cancel.lock().unwrap().as_ref() {
            token.cancel();
        }
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
    cancel: CancelSlot,
    crashes: bool,
    /// Output over this many bytes is shrunk and its original kept.
    compact_at: usize,
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
            cancel: CancelSlot::default(),
            crashes: false,
            compact_at: NEVER_COMPACT,
        }
    }

    /// Has the tool cancel `lp` as it answers.
    fn cancel_when_the_tool_runs(&self, lp: &Loop) {
        *self.cancel.lock().unwrap() = Some(lp.cancel_token());
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

    /// The same, for a session whose `session_start` hook passed in an earlier loop.
    fn resumed_loop(&self, hooks: &[HookConfig]) -> (Loop, UnboundedReceiver<TurnEvent>) {
        let (lp, events) = self.history_only_loop(hooks);
        (lp.with_resumed(true), events)
    }

    /// A loop for a conversation that has messages, whose session has not
    /// passed a `session_start` hook, as when its first send was blocked.
    fn history_only_loop(&self, hooks: &[HookConfig]) -> (Loop, UnboundedReceiver<TurnEvent>) {
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
            cancel: Arc::clone(&self.cancel),
            crashes: self.crashes,
        }));
        let config = TurnConfig {
            session: self.session.clone(),
            cwd: self.scratch.out("ws"),
            max_turns: 8,
            token_limit: 1_000_000,
            compact_threshold_bytes: self.compact_at,
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
async fn a_conversation_with_history_is_not_resumed_when_no_session_start_ever_passed() {
    // Arrange
    let f = Fixture::new();
    let log = f.scratch.out("history.json");
    let hook = shell(
        HookEvent::SessionStart,
        &format!("cat >> {0}\necho >> {0}", log.display()),
        false,
    );
    let (mut lp, _events) = f.history_only_loop(&[hook]);

    // Act
    send(&mut lp, "one").await;

    // Assert
    let seen = std::fs::read_to_string(&log).unwrap();
    let payload: Value = serde_json::from_str(seen.lines().next().unwrap()).unwrap();
    assert_eq!(payload["resumed"], false, "{payload}");
}

#[tokio::test]
async fn a_session_start_that_passes_is_announced_once_and_only_where_a_hook_is_set() {
    // Arrange
    let f = Fixture::new();
    let (mut with_hook, mut announced) = f.turn_loop(
        vec![end_turn(), end_turn()],
        &[shell(HookEvent::SessionStart, "exit 0", false)],
        "ran",
    );
    let (mut without, mut silent) = f.turn_loop(vec![end_turn()], &[], "ran");
    let (mut blocked, mut refused) = f.turn_loop(
        vec![],
        &[shell(HookEvent::SessionStart, "exit 1", true)],
        "ran",
    );
    let (mut resumed, mut known) =
        f.resumed_loop(&[shell(HookEvent::SessionStart, "exit 0", false)]);

    // Act
    send(&mut with_hook, "one").await;
    send(&mut with_hook, "two").await;
    send(&mut without, "one").await;
    send(&mut blocked, "one").await;
    send(&mut resumed, "one").await;

    // Assert
    let count = |events: &mut UnboundedReceiver<TurnEvent>| {
        let mut started = 0;
        while let Ok(event) = events.try_recv() {
            started += usize::from(event == TurnEvent::SessionStarted);
        }
        started
    };
    assert_eq!(count(&mut announced), 1);
    assert_eq!(count(&mut silent), 0);
    assert_eq!(count(&mut refused), 0);
    // It is stored already, so a loop that was told so does not store it again.
    assert_eq!(count(&mut known), 0);
}

/// Sends `text`, and cancels it once the hook has written its pid to `pid_file`.
/// Returns how it stopped and how long after the cancel the send ended.
async fn cancelled_once_the_hook_runs(
    lp: &mut Loop,
    text: &str,
    pid_file: &Path,
) -> (StopCondition, Duration) {
    let cancel = lp.cancel_token();
    let cancel_once_running = async {
        let _ = pid_from(pid_file).await;
        cancel.cancel();
        Instant::now()
    };
    let sent = async {
        let stop = send(lp, text).await;
        (stop, Instant::now())
    };
    let ((stop, ended), cancelled) = tokio::join!(sent, cancel_once_running);
    (stop, ended.saturating_duration_since(cancelled))
}

/// A `cat` hook that keeps what it is given in `file`.
fn capture(event: HookEvent, file: &Path) -> HookConfig {
    shell(event, &format!("cat > {}", file.display()), false)
}

/// A hook of `event` that writes its pid to `pid_file` and then sleeps.
fn sleeping_hook(event: HookEvent, pid_file: &Path, blocking: bool) -> HookConfig {
    let script = format!("echo $$ > {}\nsleep 30", pid_file.display());
    shell(event, &script, blocking)
}

#[tokio::test]
async fn cancelling_while_a_blocking_hook_runs_ends_the_turn_and_kills_the_hook() {
    // Arrange
    let f = Fixture::new();
    let pid_file = f.scratch.out("hook.pid");
    let hooks = [sleeping_hook(HookEvent::PreTool, &pid_file, true)];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, "ran");

    // Act
    let (stop, after_cancel) = cancelled_once_the_hook_runs(&mut lp, "go", &pid_file).await;

    // Assert
    assert_eq!(stop, StopCondition::Cancelled);
    assert!(after_cancel < PROMPT, "the send took {after_cancel:?}");
    assert_eq!(f.tool_ran(), 0);
    let hook_pid = pid_from(&pid_file).await;
    poll_until(|| !process_alive(&hook_pid)).await;
}

#[tokio::test]
async fn cancelling_while_a_session_start_hook_runs_ends_the_turn_before_the_model() {
    // Arrange
    let f = Fixture::new();
    let pid_file = f.scratch.out("hook.pid");
    let hooks = [sleeping_hook(HookEvent::SessionStart, &pid_file, true)];
    // No scripted turn: a call to the model would fail the send.
    let (mut lp, _events) = f.turn_loop(Vec::new(), &hooks, "ran");

    // Act
    let (stop, after_cancel) = cancelled_once_the_hook_runs(&mut lp, "go", &pid_file).await;

    // Assert
    assert_eq!(stop, StopCondition::Cancelled);
    assert!(after_cancel < PROMPT, "the send took {after_cancel:?}");
    let hook_pid = pid_from(&pid_file).await;
    poll_until(|| !process_alive(&hook_pid)).await;
}

#[tokio::test]
async fn cancelling_while_a_blocking_post_tool_hook_runs_withholds_the_result_and_kills_the_hook() {
    // Arrange
    let f = Fixture::new();
    let pid_file = f.scratch.out("hook.pid");
    let hooks = [sleeping_hook(HookEvent::PostTool, &pid_file, true)];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, "SECRET-OUTPUT");

    // Act
    let (stop, after_cancel) = cancelled_once_the_hook_runs(&mut lp, "go", &pid_file).await;

    // Assert
    assert_eq!(stop, StopCondition::Cancelled);
    assert!(after_cancel < PROMPT, "the send took {after_cancel:?}");
    assert_eq!(f.tool_ran(), 1, "the tool ran before the hook");
    let results = results(&lp);
    assert_eq!(results.len(), 1);
    assert!(results[0].1, "the model must see an error: {results:?}");
    assert!(!results[0].0.contains("SECRET-OUTPUT"), "{results:?}");
    assert!(results[0].0.contains("cancelled"), "{results:?}");
    let hook_pid = pid_from(&pid_file).await;
    poll_until(|| !process_alive(&hook_pid)).await;
}

#[tokio::test]
async fn a_cancel_that_comes_with_the_tools_result_withholds_it_from_a_blocking_post_tool_hook() {
    // Arrange
    let f = Fixture::new();
    // A hook that would pass, but a cancel that is already set leaves it no time to run.
    let hooks = [shell(HookEvent::PostTool, "exit 0", true)];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, "SECRET-OUTPUT");
    f.cancel_when_the_tool_runs(&lp);

    // Act
    let stop = send(&mut lp, "go").await;

    // Assert
    assert_eq!(stop, StopCondition::Cancelled);
    let results = results(&lp);
    assert!(results[0].1, "the model must see an error: {results:?}");
    assert!(!results[0].0.contains("SECRET-OUTPUT"), "{results:?}");
}

#[tokio::test]
async fn a_cancel_during_a_post_tool_hook_that_is_not_blocking_keeps_the_result() {
    // Arrange
    let f = Fixture::new();
    let hooks = [shell(HookEvent::PostTool, "sleep 30", false)];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, "SECRET-OUTPUT");
    f.cancel_when_the_tool_runs(&lp);

    // Act
    let stop = send(&mut lp, "go").await;

    // Assert
    assert_eq!(stop, StopCondition::Cancelled);
    assert_eq!(results(&lp), [("SECRET-OUTPUT".to_owned(), false)]);
}

#[tokio::test]
async fn a_tool_that_crashes_still_goes_through_the_post_tool_hooks() {
    // Arrange
    let mut f = Fixture::new();
    f.crashes = true;
    let seen = f.scratch.out("seen.json");
    let hooks = [capture(HookEvent::PostTool, &seen)];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, "ran");

    // Act
    let stop = send(&mut lp, "go").await;

    // Assert
    assert_eq!(stop, StopCondition::EndTurn);
    let payload: Value = serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
    assert_eq!(payload["is_error"], true);
    let result = payload["result"].as_str().unwrap();
    assert!(result.contains("the canned tool crashed"), "{result}");
}

#[tokio::test]
async fn a_blocking_post_tool_hook_can_replace_the_text_of_a_crashed_tool() {
    // Arrange
    let mut f = Fixture::new();
    f.crashes = true;
    let script = "echo 'crash text vetoed' >&2\nexit 1";
    let hooks = [shell(HookEvent::PostTool, script, true)];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, "ran");

    // Act
    send(&mut lp, "go").await;

    // Assert
    let results = results(&lp);
    assert!(results[0].1);
    assert!(results[0].0.contains("crash text vetoed"), "{results:?}");
    assert!(
        !results[0].0.contains("the canned tool crashed"),
        "{results:?}"
    );
}

/// Shrink output over this many bytes, so a big answer is stored.
const COMPACT_AT: usize = 1024;
/// The ids a test session could have handed out; far more than it stores.
const SESSION_IDS: u32 = 8;

/// An answer that shrinks a lot, so the tool's result is compacted and stored.
fn big_answer() -> String {
    "a log line\n".repeat(COMPACT_AT)
}

/// What the `retrieve` tool answers for `id`, read the way a session reads it.
fn retrieve(f: &Fixture, id: &str) -> Result<ToolOutput, ToolError> {
    let reader = StoreOriginals::new(Arc::clone(&f.store), f.session.clone());
    RetrieveTool::new(reader, usize::MAX).call(&json!({ "id": id }))
}

fn is_not_found<T>(answer: &Result<T, ToolError>) -> bool {
    matches!(answer, Err(ToolError::Original(ReaderError::NotFound(_))))
}

/// The ids of the session that `retrieve` can still answer.
fn retrievable(f: &Fixture) -> Vec<u32> {
    let found = |id: &u32| !is_not_found(&retrieve(f, &id.to_string()));
    (1..=SESSION_IDS).filter(found).collect()
}

/// The id the hook was told to `retrieve` the full output by.
fn announced_id(seen: &Path) -> String {
    let payload: Value = serde_json::from_str(&std::fs::read_to_string(seen).unwrap()).unwrap();
    let result = payload["result"].as_str().unwrap().to_owned();
    let (_, tail) = result.rsplit_once("retrieve id ").expect(&result);
    tail.trim_end_matches([']', '\n']).to_owned()
}

#[tokio::test]
async fn a_result_a_post_tool_hook_passed_keeps_its_original_to_retrieve() {
    // Arrange
    let mut f = Fixture::new();
    f.compact_at = COMPACT_AT;
    let seen = f.scratch.out("seen.json");
    let hooks = [capture(HookEvent::PostTool, &seen)];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, &big_answer());

    // Act
    send(&mut lp, "go").await;

    // Assert
    let id = announced_id(&seen);
    assert_eq!(retrieve(&f, &id).unwrap().text, big_answer());
}

#[tokio::test]
async fn a_result_a_blocking_post_tool_hook_vetoed_leaves_no_original_to_retrieve() {
    // Arrange
    let mut f = Fixture::new();
    f.compact_at = COMPACT_AT;
    let seen = f.scratch.out("seen.json");
    let script = format!("cat > {}\necho 'vetoed' >&2\nexit 1", seen.display());
    let hooks = [shell(HookEvent::PostTool, &script, true)];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, &big_answer());

    // Act
    send(&mut lp, "go").await;

    // Assert
    let results = results(&lp);
    assert!(
        results[0].1 && results[0].0.contains("vetoed"),
        "{results:?}"
    );
    let id = announced_id(&seen);
    assert!(
        is_not_found(&retrieve(&f, &id)),
        "the vetoed output is still stored under {id}"
    );
    assert_eq!(retrievable(&f), Vec::<u32>::new());
}

#[tokio::test]
async fn a_result_withheld_on_cancel_leaves_no_original_to_retrieve() {
    // Arrange
    let mut f = Fixture::new();
    f.compact_at = COMPACT_AT;
    let hooks = [shell(HookEvent::PostTool, "exit 0", true)];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, &big_answer());
    f.cancel_when_the_tool_runs(&lp);

    // Act
    let stop = send(&mut lp, "go").await;

    // Assert
    assert_eq!(stop, StopCondition::Cancelled);
    let results = results(&lp);
    assert!(
        results[0].1 && results[0].0.contains("cancelled"),
        "{results:?}"
    );
    assert_eq!(retrievable(&f), Vec::<u32>::new());
}

#[tokio::test]
async fn a_result_kept_after_a_cancel_keeps_its_original_to_retrieve() {
    // Arrange
    let mut f = Fixture::new();
    f.compact_at = COMPACT_AT;
    let hooks = [shell(HookEvent::PostTool, "sleep 30", false)];
    let (mut lp, _events) = f.turn_loop(one_call(), &hooks, &big_answer());
    f.cancel_when_the_tool_runs(&lp);

    // Act
    send(&mut lp, "go").await;

    // Assert
    let results = results(&lp);
    assert!(
        !results[0].1 && results[0].0.contains("retrieve id"),
        "{results:?}"
    );
    assert_eq!(retrievable(&f), [1]);
}
