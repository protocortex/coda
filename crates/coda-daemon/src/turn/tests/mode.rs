// SPDX-License-Identifier: Apache-2.0

//! `session.set_mode` changes what an idle session may do from its next turn.

use coda_protocol::types::{BackendKind, Mode, SessionId};
use coda_store::NewSession;
use serde_json::{Value, json};

use super::{Rig, SESSION_NOT_FOUND, TURN_RUNNING, codex_rig, relaxed};

const REPORT: &str = "report.json";
const BAN_FLAG: &str = "--disallowedTools";

async fn set_mode(rig: &mut Rig, mode: &str) -> Value {
    let params = json!({"session_id": rig.session, "mode": mode});
    rig.call("session.set_mode", params).await
}

async fn stored_mode(rig: &Rig, session: &SessionId) -> Mode {
    let id = session.clone();
    rig.hub
        .with_store(move |store| store.load_session(&id))
        .await
        .unwrap()
        .mode
}

fn args_of_last_start(rig: &Rig) -> Vec<String> {
    let raw = std::fs::read_to_string(rig.cwd.path().join(REPORT)).unwrap();
    let seen: Value = serde_json::from_str(&raw).unwrap();
    seen["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn changing_to_plan_mode_is_stored_and_answered_with_the_session() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let answer = set_mode(&mut rig, "plan").await;

    // Assert
    assert_eq!(answer["result"]["session"]["mode"], "plan");
    assert_eq!(stored_mode(&rig, &rig.session).await, Mode::Plan);
}

#[tokio::test]
async fn the_next_turn_after_a_change_to_plan_mode_bans_the_editing_tools_and_denies_requests() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let report = rig.cwd.path().join(REPORT);
    rig.control(&json!({ "report_path": report }));
    set_mode(&mut rig, "plan").await;

    // Act
    rig.send("hello").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert!(args_of_last_start(&rig).contains(&BAN_FLAG.to_owned()));
    let kinds = rig.kinds().await;
    assert!(kinds.contains(&"deny".to_owned()), "{kinds:?}");
}

#[tokio::test]
async fn a_change_back_to_default_makes_the_next_turn_ask_again() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    set_mode(&mut rig, "plan").await;
    rig.send("hello").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
    set_mode(&mut rig, "default").await;

    // Act
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;

    // Assert
    assert_eq!(stored_mode(&rig, &rig.session).await, Mode::Default);
    rig.respond(&request, "allow_once").await;
    rig.wait_until_idle().await;
}

#[tokio::test]
async fn an_agent_can_run_once_the_session_is_changed_to_plan_mode() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    set_mode(&mut rig, "plan").await;

    // Act
    let sent = rig.run_agent("reviewer", "the diff is here").await;

    // Assert
    assert!(sent["result"]["turn_id"].is_string(), "{sent}");
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
}

#[tokio::test]
async fn a_change_while_a_turn_runs_is_refused_and_keeps_the_mode() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;

    // Act
    let answer = set_mode(&mut rig, "plan").await;

    // Assert
    assert_eq!(answer["error"]["code"], TURN_RUNNING);
    assert_eq!(stored_mode(&rig, &rig.session).await, Mode::Default);
    rig.respond(&request, "allow_once").await;
    rig.wait_until_idle().await;
}

#[tokio::test]
async fn a_change_for_an_unknown_session_is_session_not_found() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let answer = rig
        .call(
            "session.set_mode",
            json!({"session_id": "999", "mode": "plan"}),
        )
        .await;

    // Assert
    assert_eq!(answer["error"]["code"], SESSION_NOT_FOUND);
}

#[tokio::test]
async fn a_model_api_session_can_change_mode_too() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let faux = rig
        .hub
        .with_store_mut(move |store| {
            store.create_session(&NewSession {
                backend: BackendKind::Faux,
                model: None,
                cwd,
                mode: Mode::Default,
            })
        })
        .await
        .unwrap();

    // Act
    let answer = rig
        .call(
            "session.set_mode",
            json!({"session_id": faux, "mode": "plan"}),
        )
        .await;

    // Assert
    assert_eq!(answer["result"]["session"]["mode"], "plan");
    assert_eq!(stored_mode(&rig, &faux).await, Mode::Plan);
}

async fn store_mode(rig: &Rig, mode: Mode) {
    let id = rig.session.clone();
    rig.hub
        .with_store_mut(move |store| store.set_session_mode(&id, mode))
        .await
        .unwrap();
}

async fn record(rig: &Rig) -> coda_store::SessionRecord {
    let id = rig.session.clone();
    rig.hub
        .with_store(move |store| store.load_session(&id))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_send_that_loaded_the_session_before_a_change_runs_under_the_stored_mode() {
    // Arrange
    let rig = Rig::new(relaxed(), Vec::new());
    let report = rig.cwd.path().join(REPORT);
    rig.control(&json!({ "report_path": report }));
    let stale = record(&rig).await;
    store_mode(&rig, Mode::Plan).await;

    // Act
    rig.turns
        .send_agent(stale, "hello".to_owned(), "hello".to_owned(), false)
        .await
        .unwrap();
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert!(args_of_last_start(&rig).contains(&BAN_FLAG.to_owned()));
    let kinds = rig.kinds().await;
    assert!(kinds.contains(&"deny".to_owned()), "{kinds:?}");
}

#[tokio::test]
async fn an_agent_run_that_checked_plan_mode_before_a_change_out_of_it_is_refused() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.use_plan_session().await;
    let stale = record(&rig).await;
    store_mode(&rig, Mode::Default).await;

    // Act
    let refused = rig
        .turns
        .send_agent(stale, "hello".to_owned(), "hello".to_owned(), true)
        .await;

    // Assert
    assert!(
        matches!(refused, Err(crate::turn::TurnError::Agent(_))),
        "{refused:?}"
    );
    assert_eq!(rig.kinds().await, Vec::<String>::new());
}

#[tokio::test]
async fn a_change_drops_the_pooled_codex_server_and_the_next_turn_starts_a_new_one() {
    // Arrange
    let mut rig = codex_rig(&json!({}));
    rig.send("hello").await;
    rig.wait_until_turn_over().await;
    let first = rig.codex_pid();

    // Act
    set_mode(&mut rig, "plan").await;
    rig.send("second").await;
    rig.wait_until_turn_over().await;

    // Assert
    assert_ne!(rig.codex_pid(), first);
}

#[tokio::test]
async fn setting_the_mode_a_session_already_has_keeps_what_the_daemon_holds() {
    // Arrange
    let mut rig = codex_rig(&json!({}));
    rig.send("hello").await;
    rig.wait_until_turn_over().await;
    let first = rig.codex_pid();

    // Act
    let answer = set_mode(&mut rig, "default").await;
    rig.send("second").await;
    rig.wait_until_turn_over().await;

    // Assert
    assert_eq!(answer["result"]["session"]["mode"], "default");
    assert_eq!(rig.codex_pid(), first);
}
