// SPDX-License-Identifier: Apache-2.0

//! The backend's conversation id survives a daemon restart.

use serde_json::{Value, json};

use super::{Rig, relaxed};
use coda_protocol::types::{BackendKind, TurnId};

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
            store.record_backend_thread(
                &session,
                BackendKind::ClaudeCode,
                STALE_ID,
                &TurnId::new("0"),
            )
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
    assert_eq!(rig.count("backend_crashed").await, 1);
    let session = rig.session.clone();
    let stored = rig
        .hub
        .with_store(move |store| store.active_backend_ref(&session, BackendKind::ClaudeCode))
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some(BACKEND_ID));
    let failed: i64 = rig
        .db()
        .query_row(
            "SELECT count(*) FROM backend_threads WHERE status = 'error'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(failed, 0, "the new id replaced the failed row");
}
