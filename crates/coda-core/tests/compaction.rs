// SPDX-License-Identifier: AGPL-3.0-only

//! Compaction scenarios: a conversation that outgrows the model window is
//! summarized by the same provider, then the request is retried once.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coda_core::compact::{KEEP_RECENT_MESSAGES, summary_message};
use coda_core::prompt::{Conversation, Message, Prefix};
use coda_core::turn::{StopCondition, ToolSet, TurnConfig, TurnError, TurnEvent, TurnLoop};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{
    ModelEvent, ModelProvider, ModelRequest, ModelStream, ProviderError, RequestMessage, StopReason,
};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store};
use coda_tools::{Tool, ToolError, ToolOutput};
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedReceiver;

const BOUND: Duration = Duration::from_secs(10);
const SYSTEM: &str = "You are Coda.";
const WINDOW: u32 = 1000;
/// Small enough that a long prompt nears it.
const SMALL_WINDOW: u32 = 100;
/// Characters of the window one summarizer request may carry.
const WINDOW_BYTES: usize = 4000;
const BIG_TEXT_CHARS: usize = 1200;
const STORED: &str = "stored";
const REQUEST: &str = "request";

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
            text: "ran".to_owned(),
            truncated: false,
        })
    }
}

/// Remembers every request and what happened around it, and can hang on one.
struct Recording {
    inner: FauxProvider,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    log: Arc<Mutex<Vec<&'static str>>>,
    /// The index of the request that never answers.
    hang_at: Option<usize>,
    hanging: Arc<Notify>,
    held: Mutex<Vec<tokio::sync::mpsc::Sender<coda_providers::model::StreamItem>>>,
}

impl ModelProvider for Recording {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProviderError> {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            requests.len() - 1
        };
        self.log.lock().unwrap().push(REQUEST);
        if self.hang_at == Some(index) {
            let (sender, stream) = ModelStream::channel(1);
            self.held.lock().unwrap().push(sender);
            self.hanging.notify_one();
            return Ok(stream);
        }
        self.inner.stream(request).await
    }
}

struct Rig {
    _dir: TempDir,
    lp: TurnLoop<Recording, AllowAll>,
    events: UnboundedReceiver<TurnEvent>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    log: Arc<Mutex<Vec<&'static str>>>,
    hanging: Arc<Notify>,
    /// What the daemon the rig plays answers to a compaction: stored or
    /// not, or nothing at all, which cancels the send while it waits.
    answer: Option<bool>,
}

impl Rig {
    fn new(turns: Vec<Turn>, window: Option<u32>, hang_at: Option<usize>) -> Self {
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
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let hanging = Arc::new(Notify::new());
        let provider = Recording {
            inner: FauxProvider::new(turns),
            requests: Arc::clone(&requests),
            log: Arc::clone(&log),
            hang_at,
            hanging: Arc::clone(&hanging),
            held: Mutex::default(),
        };
        let (sender, events) = tokio::sync::mpsc::unbounded_channel();
        let lp = TurnLoop::new(
            provider,
            AllowAll,
            ToolSet::new().with(Arc::new(Canned)),
            Arc::new(Mutex::new(store)),
            Conversation::new(Prefix::new(SYSTEM, Vec::new(), "").unwrap()),
            TurnConfig {
                session,
                cwd: "/ws".into(),
                max_turns: 8,
                token_limit: 1_000_000,
                compact_threshold_bytes: 1024,
            },
            sender,
        )
        .with_context_window(window);
        Self {
            _dir: dir,
            lp,
            events,
            requests,
            log,
            hanging,
            answer: Some(true),
        }
    }

    /// Sends `text` and plays the daemon: every compaction is confirmed as
    /// stored, and a request that hangs gets the send cancelled.
    async fn send(&mut self, text: &str) -> (Result<StopCondition, TurnError>, Vec<TurnEvent>) {
        let mut seen = Vec::new();
        let Self {
            lp,
            events,
            log,
            hanging,
            answer,
            ..
        } = self;
        let cancel = lp.cancel_token();
        let outcome = {
            let send = lp.send(text);
            tokio::pin!(send);
            tokio::time::timeout(BOUND, async {
                loop {
                    tokio::select! {
                        outcome = &mut send => break outcome,
                        Some(event) = events.recv() => {
                            if let TurnEvent::Compaction { ack, .. } = &event {
                                log.lock().unwrap().push(STORED);
                                match answer {
                                    Some(stored) => ack.confirm(*stored),
                                    None => cancel.cancel(),
                                }
                            }
                            seen.push(event);
                        }
                        () = hanging.notified() => cancel.cancel(),
                    }
                }
            })
            .await
            .expect("the send did not finish in time")
        };
        while let Ok(event) = events.try_recv() {
            seen.push(event);
        }
        (outcome, seen)
    }

    async fn converse(&mut self, questions: &[&str]) {
        for question in questions {
            let (stop, _) = self.send(question).await;
            assert_eq!(stop.unwrap(), StopCondition::EndTurn);
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn log(&self) -> Vec<&'static str> {
        self.log.lock().unwrap().clone()
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

fn tool_turn(ids: &[String]) -> Turn {
    let mut events = Vec::new();
    for id in ids {
        events.push(ModelEvent::ToolCallStart {
            call_id: id.clone(),
            name: "canned".into(),
        });
        events.push(ModelEvent::ToolCallArgumentsDelta {
            call_id: id.clone(),
            fragment: "{}".into(),
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

fn failing(error: ProviderError) -> Turn {
    Turn {
        events: Vec::new(),
        error: Some(error),
    }
}

fn overflow() -> Turn {
    failing(ProviderError::ContextWindowExceeded)
}

fn compactions(events: &[TurnEvent]) -> Vec<(String, usize, usize)> {
    events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::Compaction {
                summary,
                replaced,
                kept,
                ..
            } => Some((summary.clone(), *replaced, *kept)),
            _ => None,
        })
        .collect()
}

fn user_text(message: &RequestMessage) -> &str {
    match message {
        RequestMessage::User(text) => text,
        other => panic!("expected a user message, got {other:?}"),
    }
}

/// Five plain exchanges: eleven messages once the sixth question is asked.
const FIVE: [&str; 5] = ["q1", "q2", "q3", "q4", "q5"];

fn five_answers() -> Vec<Turn> {
    ["a1", "a2", "a3", "a4", "a5"].map(text_turn).to_vec()
}

fn script(mut head: Vec<Turn>, tail: Vec<Turn>) -> Vec<Turn> {
    head.extend(tail);
    head
}

#[tokio::test]
async fn an_overflow_compacts_once_retries_once_and_succeeds() {
    // Arrange
    let turns = script(
        five_answers(),
        vec![overflow(), text_turn("SUMMARY"), text_turn("final")],
    );
    let mut rig = Rig::new(turns, None, None);
    rig.converse(&FIVE).await;

    // Act
    let (stop, events) = rig.send("q6").await;

    // Assert
    assert_eq!(stop.unwrap(), StopCondition::EndTurn);
    let requests = rig.requests();
    assert_eq!(requests.len(), 8, "five sends, overflow, summary, retry");
    let (overflowed, summarizer, retry) = (&requests[5], &requests[6], &requests[7]);
    assert_eq!(overflowed.messages.len(), 11);
    assert_eq!(summarizer.messages.len(), 1);
    assert_ne!(summarizer.system, SYSTEM);
    assert_eq!(summarizer.tools.len(), 0);
    let older = user_text(&summarizer.messages[0]);
    assert!(older.contains("q1") && older.contains("q3"), "{older}");
    assert!(
        !older.contains("a3"),
        "the kept messages are not summarized"
    );
    assert_eq!(
        retry.messages[0],
        RequestMessage::User(summary_message("SUMMARY"))
    );
    assert_eq!(retry.system, SYSTEM, "the summary is not a system prompt");
    assert_eq!(retry.messages[1..], overflowed.messages[5..]);
    assert_eq!(compactions(&events), [("SUMMARY".to_owned(), 5, 6)]);
    assert_eq!(rig.lp.conversation().messages().len(), 8);
    assert_eq!(
        rig.log()[5..],
        [REQUEST, REQUEST, STORED, REQUEST],
        "the event is stored before the retry"
    );
}

#[tokio::test]
async fn a_second_overflow_after_compaction_ends_the_turn_as_an_overflow() {
    // Arrange
    let turns = script(
        five_answers(),
        vec![overflow(), text_turn("SUMMARY"), overflow()],
    );
    let mut rig = Rig::new(turns, None, None);
    rig.converse(&FIVE).await;

    // Act
    let (stop, events) = rig.send("q6").await;

    // Assert
    assert_eq!(stop.unwrap(), StopCondition::ContextOverflow);
    assert_eq!(compactions(&events).len(), 1);
    assert!(events.contains(&TurnEvent::ContextOverflow));
    assert_eq!(rig.requests().len(), 8, "no second compaction or retry");
}

#[tokio::test]
async fn a_summarizer_failure_ends_the_turn_with_its_own_stop() {
    // Arrange
    let broken = ProviderError::Fatal {
        message: "refused".to_owned(),
    };
    let turns = script(five_answers(), vec![overflow(), failing(broken)]);
    let mut rig = Rig::new(turns, None, None);
    rig.converse(&FIVE).await;

    // Act
    let (stop, events) = rig.send("q6").await;

    // Assert
    let stop = stop.unwrap();
    assert_eq!(stop, StopCondition::CompactionFailed);
    assert_eq!(stop.as_str(), "compaction_failed");
    assert_eq!(compactions(&events), Vec::new());
    assert!(!events.contains(&TurnEvent::ContextOverflow));
    let messages = rig.lp.conversation().messages();
    assert_eq!(messages.len(), 12, "history is untouched, then closed");
    assert!(matches!(messages.last(), Some(Message::Assistant { .. })));
}

#[tokio::test]
async fn a_compaction_the_daemon_could_not_store_ends_the_turn_without_a_retry() {
    // Arrange
    let turns = script(
        five_answers(),
        vec![overflow(), text_turn("SUMMARY"), text_turn("final")],
    );
    let mut rig = Rig::new(turns, None, None);
    rig.converse(&FIVE).await;
    rig.answer = Some(false);

    // Act
    let (stop, _) = rig.send("q6").await;

    // Assert
    assert_eq!(stop.unwrap(), StopCondition::CompactionFailed);
    assert_eq!(rig.requests().len(), 7, "no request after the summary");
    assert_eq!(rig.lp.conversation().messages().len(), 12);
}

#[tokio::test]
async fn a_cancel_while_the_compaction_is_being_stored_cancels_the_turn() {
    // Arrange
    let turns = script(
        five_answers(),
        vec![overflow(), text_turn("SUMMARY"), text_turn("final")],
    );
    let mut rig = Rig::new(turns, None, None);
    rig.converse(&FIVE).await;
    rig.answer = None;

    // Act
    let (stop, _) = rig.send("q6").await;

    // Assert
    assert_eq!(stop.unwrap(), StopCondition::Cancelled);
    assert_eq!(rig.requests().len(), 7, "no request after the summary");
    assert_eq!(rig.lp.conversation().messages().len(), 12);
}

#[tokio::test]
async fn compacting_ahead_is_tried_once_per_send_even_after_it_failed() {
    // Arrange
    let broken = ProviderError::Fatal {
        message: "refused".to_owned(),
    };
    let turns = vec![
        text_turn("a1"),
        text_turn("a2"),
        text_turn("a3"),
        failing(broken),
        tool_turn(&["c1".to_owned()]),
        text_turn("final"),
    ];
    let mut rig = Rig::new(turns, Some(SMALL_WINDOW), None);
    rig.converse(&["q1", "q2", "q3"]).await;

    // Act
    let (stop, events) = rig.send(&"long ".repeat(SMALL_WINDOW as usize)).await;

    // Assert
    assert_eq!(stop.unwrap(), StopCondition::EndTurn);
    assert_eq!(compactions(&events), Vec::new());
    assert_eq!(
        rig.requests().len(),
        6,
        "three sends, one failed summary, the tool round, the answer"
    );
}

#[tokio::test]
async fn a_cancel_during_compaction_stops_the_turn_and_changes_nothing() {
    // Arrange
    let turns = script(five_answers(), vec![overflow()]);
    let mut rig = Rig::new(turns, None, Some(6));
    rig.converse(&FIVE).await;

    // Act
    let (stop, events) = rig.send("q6").await;

    // Assert
    assert_eq!(stop.unwrap(), StopCondition::Cancelled);
    assert_eq!(compactions(&events), Vec::new());
    assert_eq!(rig.requests().len(), 7, "the summarizer was the last call");
    assert_eq!(rig.lp.conversation().messages().len(), 12);
}

#[tokio::test]
async fn a_tool_call_and_its_results_stay_together_across_the_split() {
    // Arrange
    let ids: Vec<String> = (0..KEEP_RECENT_MESSAGES).map(|n| format!("c{n}")).collect();
    let turns = vec![
        text_turn("a0"),
        tool_turn(&ids),
        overflow(),
        text_turn("SUMMARY"),
        text_turn("final"),
    ];
    let mut rig = Rig::new(turns, None, None);
    rig.converse(&["q0"]).await;

    // Act
    let (stop, events) = rig.send("q1").await;

    // Assert
    assert_eq!(stop.unwrap(), StopCondition::EndTurn);
    let requests = rig.requests();
    let (overflowed, retry) = (&requests[2], &requests[4]);
    let first_result = overflowed.messages.len() - KEEP_RECENT_MESSAGES;
    assert!(matches!(
        overflowed.messages[first_result],
        RequestMessage::ToolResult { .. }
    ));
    let call_at = first_result - 1;
    assert_eq!(retry.messages[1..], overflowed.messages[call_at..]);
    assert_eq!(compactions(&events)[0].1, call_at);
}

#[tokio::test]
async fn a_request_near_the_window_is_compacted_in_chunks_before_it_is_sent() {
    // Arrange
    let big = "x".repeat(BIG_TEXT_CHARS);
    let turns = vec![
        text_turn(&"y".repeat(BIG_TEXT_CHARS)),
        text_turn("a2"),
        text_turn("a3"),
        text_turn("a4"),
        text_turn("SUM-1"),
        text_turn("SUM-2"),
        text_turn("final"),
    ];
    let mut rig = Rig::new(turns, Some(WINDOW), None);
    rig.converse(&[big.as_str(), "q2", "q3", "q4"]).await;

    // Act
    let (stop, events) = rig.send(&"z".repeat(BIG_TEXT_CHARS)).await;

    // Assert
    assert_eq!(stop.unwrap(), StopCondition::EndTurn);
    let requests = rig.requests();
    assert_eq!(requests.len(), 7, "four sends, two chunks, the request");
    let (first, second) = (
        user_text(&requests[4].messages[0]),
        user_text(&requests[5].messages[0]),
    );
    assert!(first.len() < WINDOW_BYTES && second.len() < WINDOW_BYTES);
    assert!(!first.contains("SUM-1") && second.contains("SUM-1"));
    assert_eq!(
        requests[6].messages[0],
        RequestMessage::User(summary_message("SUM-2"))
    );
    assert_eq!(compactions(&events).len(), 1);
}

#[tokio::test]
async fn a_model_with_no_known_window_is_not_compacted_ahead_of_time() {
    // Arrange
    let turns = vec![
        text_turn(&"y".repeat(BIG_TEXT_CHARS)),
        text_turn("a2"),
        text_turn("a3"),
        text_turn("a4"),
        text_turn("final"),
    ];
    let mut rig = Rig::new(turns, None, None);
    let big = "x".repeat(BIG_TEXT_CHARS);
    rig.converse(&[big.as_str(), "q2", "q3", "q4"]).await;

    // Act
    let (stop, events) = rig.send(&"z".repeat(BIG_TEXT_CHARS)).await;

    // Assert
    assert_eq!(stop.unwrap(), StopCondition::EndTurn);
    assert_eq!(rig.requests().len(), 5);
    assert_eq!(compactions(&events), Vec::new());
}
