// SPDX-License-Identifier: Apache-2.0

//! The backend's conversation id survives a daemon restart.

use serde_json::{Value, json};

use super::{Rig, relaxed, timing};
use coda_protocol::types::BackendKind;

const REPORT: &str = "report.json";
const RESUME_FLAG: &str = "--resume";
const BACKEND_ID: &str = "fake-session-1";
const STALE_ID: &str = "stale-id";
const NO_PERMISSION_TURN: &str = "tool-slow";

fn args(rig: &Rig) -> Vec<String> {
    let raw = std::fs::read_to_string(rig.cwd.path().join(REPORT)).unwrap();
    let report: Value = serde_json::from_str(&raw).unwrap();
    serde_json::from_value(report["args"].clone()).unwrap()
}

fn resumed_with(args: &[String]) -> Option<&str> {
    let at = args.iter().position(|a| a == RESUME_FLAG)?;
    args.get(at + 1).map(String::as_str)
}

#[tokio::test]
async fn a_restarted_daemon_resumes_the_backend_conversation_of_the_session() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.control(&json!({ "report_path": rig.cwd.path().join(REPORT) }));
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
    let binary = coda_testkit::paths::fake_claude_path().unwrap();
    let restarted = rig.turns_with(binary, rig.sleeper.clone());

    // Act
    rig.send_to(&restarted, NO_PERMISSION_TURN).await;
    rig.wait_until_stopped(&restarted).await;

    // Assert
    assert_eq!(resumed_with(&args(&rig)), Some(BACKEND_ID));
}

#[tokio::test]
async fn a_resume_the_backend_rejects_falls_back_to_a_fresh_start_and_is_not_tried_again() {
    // Arrange
    let rig = Rig::new(relaxed(), Vec::new());
    rig.control(&json!({
        "report_path": rig.cwd.path().join(REPORT),
        "reject_resume": true,
    }));
    let session = rig.session.clone();
    rig.hub
        .with_store_mut(move |store| {
            store.record_backend_thread(&session, BackendKind::ClaudeCode, STALE_ID)
        })
        .await
        .unwrap();
    let binary = coda_testkit::paths::fake_claude_path().unwrap();
    let restarted = rig.turns_with(binary, rig.sleeper.clone());

    // Act
    rig.send_to(&restarted, NO_PERMISSION_TURN).await;
    rig.wait_until_stopped(&restarted).await;

    // Assert
    assert_eq!(resumed_with(&args(&rig)), None);
    assert_eq!(
        rig.count("backend_crashed").await,
        2,
        "the resume is tried twice before it counts as refused"
    );
    let session = rig.session.clone();
    let stored = rig
        .hub
        .with_store(move |store| store.active_backend_ref(&session, BackendKind::ClaudeCode))
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some(BACKEND_ID));
}

const UNREADABLE_SEQ: i64 = 1_000_000;
const PROMPTS: &str = "prompts.txt";
const STALL_TURN: &str = "stall";
const SEED_MARK: &str = "This conversation continues in a new thread";

#[tokio::test]
async fn a_resume_the_backend_rejects_sends_the_stored_conversation_with_the_prompt() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.control(&json!({ "report_path": rig.cwd.path().join(REPORT) }));
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
    rig.control(&json!({
        "report_path": rig.cwd.path().join(REPORT),
        "prompt_path": rig.cwd.path().join(PROMPTS),
        "reject_resume": true,
    }));
    let binary = coda_testkit::paths::fake_claude_path().unwrap();
    let restarted = rig.turns_with(binary, rig.sleeper.clone());

    // Act
    rig.send_to(&restarted, NO_PERMISSION_TURN).await;
    rig.wait_until_stopped(&restarted).await;

    // Assert
    let prompts = std::fs::read_to_string(rig.cwd.path().join(PROMPTS)).unwrap();
    let sent: Vec<&str> = prompts.lines().collect();
    assert_eq!(sent.len(), 1, "only the fresh start read a prompt");
    assert!(sent[0].contains(SEED_MARK));
    assert!(sent[0].contains("hello"));
    assert!(sent[0].contains("working"));
    assert_eq!(
        sent[0].matches(NO_PERMISSION_TURN).count(),
        1,
        "the new prompt is not part of the seed"
    );
}

#[tokio::test]
async fn the_backend_id_is_stored_when_the_backend_reports_it_not_when_the_turn_ends() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    rig.send(STALL_TURN).await;
    let stored = tokio::time::timeout(super::WAIT, async {
        loop {
            let session = rig.session.clone();
            let found = rig
                .hub
                .with_store(move |store| {
                    store.active_backend_ref(&session, BackendKind::ClaudeCode)
                })
                .await
                .unwrap();
            if found.is_some() {
                return found;
            }
            tokio::time::sleep(super::POLL).await;
        }
    })
    .await
    .expect("the id was not stored while the turn ran");

    // Assert
    assert_eq!(stored.as_deref(), Some(BACKEND_ID));
    assert!(rig.turns.is_running(&rig.session), "the turn is still open");
    rig.turns.cancel(&rig.session);
}

const CRASH_MARKER: &str = "resume-crashed";
/// Long enough for a turn that waits on nothing, short enough that the stalled retry ends fast.
const SHORT_IDLE_MS: u64 = 2_000;

#[tokio::test]
async fn one_crash_before_the_backend_names_its_conversation_keeps_the_stored_id() {
    // Arrange
    let rig = Rig::new(relaxed(), Vec::new());
    rig.control(&json!({
        "report_path": rig.cwd.path().join(REPORT),
        "prompt_path": rig.cwd.path().join(PROMPTS),
        "resume_crash_marker": rig.cwd.path().join(CRASH_MARKER),
    }));
    let session = rig.session.clone();
    rig.hub
        .with_store_mut(move |store| {
            store.record_backend_thread(&session, BackendKind::ClaudeCode, STALE_ID)
        })
        .await
        .unwrap();
    let binary = coda_testkit::paths::fake_claude_path().unwrap();
    let restarted = rig.turns_with(binary, rig.sleeper.clone());

    // Act
    rig.send_to(&restarted, NO_PERMISSION_TURN).await;
    rig.wait_until_stopped(&restarted).await;

    // Assert
    assert_eq!(resumed_with(&args(&rig)), Some(STALE_ID));
    let prompts = std::fs::read_to_string(rig.cwd.path().join(PROMPTS)).unwrap();
    assert!(
        !prompts.contains(SEED_MARK),
        "no history is sent on a resume"
    );
}

#[tokio::test]
async fn a_crash_after_the_fresh_start_names_its_conversation_resumes_that_conversation() {
    // Arrange
    let mut rig = Rig::new(timing(SHORT_IDLE_MS, 20_000), Vec::new());
    rig.control(&json!({ "report_path": rig.cwd.path().join(REPORT) }));
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
    let session = rig.session.clone();
    rig.hub
        .with_store_mut(move |store| {
            store.record_backend_thread(&session, BackendKind::ClaudeCode, STALE_ID)
        })
        .await
        .unwrap();
    rig.control(&json!({
        "report_path": rig.cwd.path().join(REPORT),
        "prompt_path": rig.cwd.path().join(PROMPTS),
        "reject_resume_of": STALE_ID,
        "crash_marker": rig.cwd.path().join(CRASH_MARKER),
    }));
    let binary = coda_testkit::paths::fake_claude_path().unwrap();
    let restarted = rig.turns_with(binary, rig.sleeper.clone());

    // Act
    rig.send_to(&restarted, "flaky").await;
    rig.wait_until_stopped(&restarted).await;

    // Assert
    assert_eq!(resumed_with(&args(&rig)), Some(BACKEND_ID));
    let prompts = std::fs::read_to_string(rig.cwd.path().join(PROMPTS)).unwrap();
    assert_eq!(
        prompts.matches(SEED_MARK).count(),
        1,
        "the history is sent once"
    );
}

#[tokio::test]
async fn a_fresh_start_that_cannot_read_the_stored_conversation_says_so() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.control(&json!({ "report_path": rig.cwd.path().join(REPORT) }));
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
    // A body that is not valid UTF-8 cannot be read back, while a new event still stores.
    rig.db()
        .execute(
            "INSERT INTO events (turn_id, seq, kind, body, at) \
             SELECT id, ?1, 'user_message', CAST(X'FF' AS TEXT), 0 FROM turns LIMIT 1",
            [UNREADABLE_SEQ],
        )
        .unwrap();
    rig.control(&json!({
        "report_path": rig.cwd.path().join(REPORT),
        "prompt_path": rig.cwd.path().join(PROMPTS),
        "reject_resume": true,
    }));
    let binary = coda_testkit::paths::fake_claude_path().unwrap();
    let restarted = rig.turns_with(binary, rig.sleeper.clone());

    // Act
    rig.send_to(&restarted, NO_PERMISSION_TURN).await;
    rig.wait_until_stopped(&restarted).await;

    // Assert
    let notices: i64 = rig
        .db()
        .query_row(
            "SELECT count(*) FROM events WHERE kind = 'history_not_restored'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(notices, 1);
    let prompts = std::fs::read_to_string(rig.cwd.path().join(PROMPTS)).unwrap();
    assert_eq!(
        prompts.lines().count(),
        1,
        "only the fresh start read a prompt"
    );
    assert!(!prompts.contains(SEED_MARK), "no history could be sent");
}
