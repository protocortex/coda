// SPDX-License-Identifier: Apache-2.0

//! Checks in the turn loop: when a check runs after a file change and what the
//! model is told. The runner is tested beside its code.

mod common;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::future::Future;
use std::sync::{Arc, Mutex};

use coda_config::checks::CheckConfig;
use coda_core::checks::Checks;
use coda_core::prompt::{Conversation, Message, Prefix, ToolSchema};
use coda_core::turn::{StopCondition, ToolSet, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision, SessionId};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{ModelEvent, StopReason};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store};
use coda_tools::{Tool, ToolError, ToolOutput};
use common::{BOUND, PATIENT_MS, Scratch};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

/// A threshold no test output reaches, so a tool's answer is passed whole.
const NEVER_COMPACT: usize = 1024 * 1024;
const EDITED: &str = "edited the file";
const SHORT_LIMIT_MS: u64 = 300;
const SLOW_SECONDS: &str = "5";

struct AllowAll;

impl PermissionGate for AllowAll {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::AllowOnce)
    }
}

/// A tool named like a file-changing one. It fails when the call says `fail`.
struct Changer(&'static str);

impl Tool for Changer {
    fn name(&self) -> &'static str {
        self.0
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        if input.get("fail").is_some() {
            return Err(ToolError::InvalidInput {
                tool: "edit",
                reason: "no match".into(),
            });
        }
        Ok(ToolOutput {
            text: EDITED.to_owned(),
            truncated: false,
        })
    }
}

type Loop = TurnLoop<FauxProvider, AllowAll>;

struct Fixture {
    scratch: Scratch,
    store: Arc<Mutex<Store>>,
    session: SessionId,
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
        }
    }

    /// A check for `extension` that appends a line to `runs`, then runs `script`.
    fn check(
        &self,
        extension: &str,
        script: &str,
        timeout_ms: u64,
    ) -> BTreeMap<String, CheckConfig> {
        let runs = self.scratch.out("runs");
        let body = format!("echo run >> {}; {script}", runs.display());
        let config = CheckConfig {
            command: vec!["/bin/sh".into(), "-c".into(), body],
            timeout_ms: Some(timeout_ms),
        };
        BTreeMap::from([(extension.to_owned(), config)])
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.scratch.out("runs")).map_or(0, |text| text.lines().count())
    }

    fn turn_loop(
        &self,
        turns: Vec<Turn>,
        checks: &BTreeMap<String, CheckConfig>,
    ) -> (Loop, UnboundedReceiver<TurnEvent>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let schema = ToolSchema {
            name: "edit".into(),
            description: "Changes a file.".into(),
            input_schema: json!({"type": "object"}),
        };
        let prefix = Prefix::new("You are Coda.", vec![schema], "").unwrap();
        let tools = ToolSet::new()
            .with(Arc::new(Changer("edit")))
            .with(Arc::new(Changer("write")));
        let config = TurnConfig {
            session: self.session.clone(),
            cwd: self.scratch.out("ws"),
            max_turns: 8,
            token_limit: 1_000_000,
            compact_threshold_bytes: NEVER_COMPACT,
        };
        let env = vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))];
        let lp = TurnLoop::new(
            FauxProvider::new(turns),
            AllowAll,
            tools,
            Arc::clone(&self.store),
            Conversation::new(prefix),
            config,
            sender,
        )
        .with_checks(Checks::new(checks, env));
        (lp, receiver)
    }
}

fn call(name: &str, id: &str, input: &Value) -> Vec<ModelEvent> {
    vec![
        ModelEvent::ToolCallStart {
            call_id: id.into(),
            name: name.into(),
        },
        ModelEvent::ToolCallArgumentsDelta {
            call_id: id.into(),
            fragment: input.to_string(),
        },
    ]
}

/// One reply that makes `calls` (tool, id, input) and one that ends the turn.
fn reply(calls: &[(&str, &str, Value)]) -> Vec<Turn> {
    let mut events: Vec<ModelEvent> = calls
        .iter()
        .flat_map(|(name, id, input)| call(name, id, input))
        .collect();
    events.push(ModelEvent::Stop {
        reason: StopReason::ToolUse,
    });
    let end = Turn {
        events: vec![
            ModelEvent::TextDelta {
                text: "done".into(),
            },
            ModelEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ],
        error: None,
    };
    vec![
        Turn {
            events,
            error: None,
        },
        end,
    ]
}

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

async fn send(lp: &mut Loop) -> StopCondition {
    tokio::time::timeout(BOUND, lp.send("go"))
        .await
        .expect("the send did not finish in time")
        .unwrap()
}

#[tokio::test]
async fn an_edit_that_breaks_the_check_returns_the_error_text_in_the_same_result() {
    // Arrange
    let f = Fixture::new();
    let checks = f.check(
        "rs",
        "echo 'error[E0425]: cannot find value' >&2; exit 101",
        PATIENT_MS,
    );
    let (mut lp, _events) = f.turn_loop(
        reply(&[("edit", "c1", json!({"path": "src/a.rs"}))]),
        &checks,
    );

    // Act
    let stop = send(&mut lp).await;

    // Assert
    assert_eq!(stop, StopCondition::EndTurn);
    let results = results(&lp);
    assert_eq!(results.len(), 1);
    assert!(!results[0].1, "a failed check does not fail the edit");
    assert!(results[0].0.starts_with(EDITED), "{results:?}");
    assert!(results[0].0.contains("check failed"), "{results:?}");
    assert!(results[0].0.contains("cannot find value"), "{results:?}");
}

#[tokio::test]
async fn a_passing_check_adds_one_short_line() {
    // Arrange
    let f = Fixture::new();
    let checks = f.check("rs", "echo 'lots of noise'", PATIENT_MS);
    let (mut lp, _events) = f.turn_loop(
        reply(&[("write", "c1", json!({"path": "src/a.rs"}))]),
        &checks,
    );

    // Act
    send(&mut lp).await;

    // Assert
    let results = results(&lp);
    assert_eq!(results[0].0.lines().count(), 2, "{results:?}");
    assert!(results[0].0.contains("check passed"), "{results:?}");
    assert!(!results[0].0.contains("noise"), "{results:?}");
}

#[tokio::test]
async fn a_check_that_times_out_is_reported_and_does_not_fail_the_edit() {
    // Arrange
    let f = Fixture::new();
    let checks = f.check("rs", &format!("sleep {SLOW_SECONDS}"), SHORT_LIMIT_MS);
    let (mut lp, _events) = f.turn_loop(reply(&[("edit", "c1", json!({"path": "a.rs"}))]), &checks);

    // Act
    send(&mut lp).await;

    // Assert
    let results = results(&lp);
    assert!(!results[0].1);
    assert!(results[0].0.contains("timed out"), "{results:?}");
    assert!(results[0].0.starts_with(EDITED), "{results:?}");
}

#[tokio::test]
async fn no_check_runs_for_an_unconfigured_extension() {
    // Arrange
    let f = Fixture::new();
    let checks = f.check("rs", "exit 1", PATIENT_MS);
    let (mut lp, _events) = f.turn_loop(
        reply(&[("edit", "c1", json!({"path": "notes.md"}))]),
        &checks,
    );

    // Act
    send(&mut lp).await;

    // Assert
    assert_eq!(f.runs(), 0);
    assert_eq!(results(&lp)[0].0, EDITED);
}

#[tokio::test]
async fn two_edits_in_one_reply_run_the_check_once_after_the_last() {
    // Arrange
    let f = Fixture::new();
    let checks = f.check("rs", "exit 0", PATIENT_MS);
    let calls = [
        ("edit", "c1", json!({"path": "a.rs"})),
        ("write", "c2", json!({"path": "b.rs"})),
    ];
    let (mut lp, _events) = f.turn_loop(reply(&calls), &checks);

    // Act
    send(&mut lp).await;

    // Assert
    assert_eq!(f.runs(), 1);
    let results = results(&lp);
    assert_eq!(results[0].0, EDITED, "the first result is untouched");
    assert!(results[1].0.contains("check passed"), "{results:?}");
}

#[tokio::test]
async fn a_failed_edit_runs_no_check() {
    // Arrange
    let f = Fixture::new();
    let checks = f.check("rs", "exit 0", PATIENT_MS);
    let (mut lp, _events) = f.turn_loop(
        reply(&[("edit", "c1", json!({"path": "a.rs", "fail": true}))]),
        &checks,
    );

    // Act
    send(&mut lp).await;

    // Assert
    assert_eq!(f.runs(), 0);
    assert!(results(&lp)[0].1);
}

#[tokio::test]
async fn an_earlier_edit_is_checked_when_the_last_edit_of_the_reply_fails() {
    // Arrange
    let f = Fixture::new();
    let checks = f.check("rs", "exit 0", PATIENT_MS);
    let calls = [
        ("edit", "c1", json!({"path": "a.rs"})),
        ("edit", "c2", json!({"path": "b.rs", "fail": true})),
    ];
    let (mut lp, _events) = f.turn_loop(reply(&calls), &checks);

    // Act
    send(&mut lp).await;

    // Assert
    assert_eq!(f.runs(), 1);
    let results = results(&lp);
    assert!(results[1].1, "the failed edit stays an error");
    assert!(results[1].0.contains("check passed"), "{results:?}");
}
