// SPDX-License-Identifier: Apache-2.0

//! `session.fork` starts a new session from a turn of an existing one.

use coda_protocol::jsonrpc::INVALID_PARAMS;
use coda_protocol::types::{BackendKind, SessionId};
use serde_json::{Value, json};

use super::{Rig, SESSION_NOT_FOUND, relaxed};

const NO_PERMISSION_TURN: &str = "tool-slow";
const PROMPT_LOG: &str = "prompts.log";
const RESUME_FLAG: &str = "--resume";
const REPORT: &str = "report.json";

async fn finished_turn(rig: &mut Rig, text: &str) {
    rig.send(text).await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
}

async fn fork(rig: &mut Rig, params: Value) -> Value {
    let mut params = params;
    let session = rig.session.clone();
    params
        .as_object_mut()
        .unwrap()
        .entry("session_id")
        .or_insert_with(|| json!(session));
    rig.call("session.fork", params).await
}

fn read(rig: &Rig, name: &str) -> Option<String> {
    std::fs::read_to_string(rig.cwd.path().join(name)).ok()
}

async fn first_turn_of(rig: &Rig) -> Value {
    let id = rig.session.clone();
    let turns = rig
        .hub
        .with_store(move |store| store.turn_checkpoints(&id))
        .await
        .unwrap();
    json!(turns[0].turn)
}

fn logging_rig() -> Rig {
    let rig = Rig::new(relaxed(), Vec::new());
    rig.control(&json!({
        "report_path": rig.cwd.path().join(REPORT),
        "prompt_path": rig.cwd.path().join(PROMPT_LOG),
    }));
    rig
}

#[tokio::test]
async fn a_fork_is_a_new_session_that_names_its_parent_and_the_latest_turn() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    let parent = rig.session.clone();

    // Act
    let answer = fork(&mut rig, json!({})).await;

    // Assert
    let result = &answer["result"];
    assert_ne!(result["session"]["id"], json!(parent));
    assert_eq!(result["parent_session_id"], json!(parent));
    assert_eq!(result["session"]["backend"], "claude_code");
    assert_eq!(result["session"]["cwd"], json!(rig.cwd.path()));
    let id = SessionId::new(result["session"]["id"].as_str().unwrap());
    let link = rig
        .hub
        .with_store(move |store| store.fork_of(&id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(json!(link.source_turn), result["source_turn"]);
}

#[tokio::test]
async fn the_first_send_to_a_fork_tells_claude_the_turns_up_to_the_fork_point_only() {
    // Arrange
    let mut rig = logging_rig();
    finished_turn(&mut rig, "tool-slow kept-question").await;
    finished_turn(&mut rig, "tool-slow later-question").await;
    let first = first_turn_of(&rig).await;
    let answer = fork(&mut rig, json!({"turn_id": first})).await;
    rig.session = SessionId::new(answer["result"]["session"]["id"].as_str().unwrap());

    // Act
    finished_turn(&mut rig, "tool-slow fork-question").await;

    // Assert
    let prompts = read(&rig, PROMPT_LOG).unwrap();
    let last = prompts.lines().last().unwrap();
    assert!(last.contains("fork-question"), "{last}");
    assert!(last.contains("kept-question"), "{last}");
    assert!(!last.contains("later-question"), "{last}");
    let report: Value = serde_json::from_str(&read(&rig, REPORT).unwrap()).unwrap();
    assert!(
        !report["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == RESUME_FLAG),
        "the fork starts its own conversation: {report}"
    );
}

#[tokio::test]
async fn the_second_send_to_a_fork_resumes_its_own_conversation_without_the_history() {
    // Arrange
    let mut rig = logging_rig();
    finished_turn(&mut rig, "tool-slow kept-question").await;
    let answer = fork(&mut rig, json!({})).await;
    rig.session = SessionId::new(answer["result"]["session"]["id"].as_str().unwrap());
    finished_turn(&mut rig, "tool-slow first-fork-question").await;

    // Act
    finished_turn(&mut rig, "tool-slow second-fork-question").await;

    // Assert
    let prompts = read(&rig, PROMPT_LOG).unwrap();
    let last = prompts.lines().last().unwrap();
    assert!(last.contains("second-fork-question"), "{last}");
    assert!(!last.contains("kept-question"), "{last}");
    let report: Value = serde_json::from_str(&read(&rig, REPORT).unwrap()).unwrap();
    assert!(
        report["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == RESUME_FLAG),
        "{report}"
    );
}

#[tokio::test]
async fn a_session_with_no_turn_cannot_be_forked() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let answer = fork(&mut rig, json!({})).await;

    // Assert
    assert_eq!(answer["error"]["code"], INVALID_PARAMS, "{answer}");
}

#[tokio::test]
async fn an_unknown_session_is_not_found() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let answer = fork(&mut rig, json!({"session_id": "9999"})).await;

    // Assert
    assert_eq!(answer["error"]["code"], SESSION_NOT_FOUND, "{answer}");
}

#[tokio::test]
async fn a_model_api_session_cannot_be_forked_yet() {
    // Arrange
    let mut rig = Rig::build(
        relaxed(),
        Vec::new(),
        BackendKind::Faux,
        coda_config::config::Config::default(),
    );

    // Act
    let answer = fork(&mut rig, json!({})).await;

    // Assert
    assert_eq!(answer["error"]["code"], INVALID_PARAMS, "{answer}");
    let message = answer["error"]["message"].as_str().unwrap();
    assert!(message.contains("Claude Code or Codex"), "{message}");
}

#[tokio::test]
async fn a_fork_of_a_fork_is_told_the_turns_of_both_parents() {
    // Arrange
    let mut rig = logging_rig();
    finished_turn(&mut rig, "tool-slow grandparent-question").await;
    let first = fork(&mut rig, json!({})).await;
    rig.session = SessionId::new(first["result"]["session"]["id"].as_str().unwrap());
    finished_turn(&mut rig, "tool-slow parent-question").await;
    let second = fork(&mut rig, json!({})).await;
    rig.session = SessionId::new(second["result"]["session"]["id"].as_str().unwrap());

    // Act
    finished_turn(&mut rig, "tool-slow child-question").await;

    // Assert
    let prompts = read(&rig, PROMPT_LOG).unwrap();
    let last = prompts.lines().last().unwrap();
    assert!(last.contains("grandparent-question"), "{last}");
    assert!(last.contains("parent-question"), "{last}");
    assert!(last.contains("child-question"), "{last}");
}

#[tokio::test]
async fn the_first_send_to_a_codex_fork_starts_a_thread_told_the_parent_turns() {
    // Arrange
    let mut rig = super::codex_rig(&json!({}));
    rig.send("kept-question").await;
    rig.wait_until_turn_over().await;
    let answer = fork(&mut rig, json!({})).await;
    rig.session = SessionId::new(answer["result"]["session"]["id"].as_str().unwrap());

    // Act
    rig.send("fork-question").await;
    rig.wait_until_turn_over().await;

    // Assert
    let report = rig.codex_report();
    let last = report["prompts"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .to_string();
    assert!(last.contains("fork-question"), "{last}");
    assert!(last.contains("kept-question"), "{last}");
}

async fn rewind_parent_to_first_turn(rig: &mut Rig) {
    let first = first_turn_of(rig).await;
    let params = json!({"session_id": rig.session, "turn_id": first, "keep_files": true});
    let answer = rig.call("session.rewind", params).await;
    assert!(answer["error"].is_null(), "{answer}");
}

#[tokio::test]
async fn a_rewind_of_the_parent_after_the_fork_does_not_change_what_the_fork_is_told() {
    // Arrange
    let mut rig = logging_rig();
    finished_turn(&mut rig, "tool-slow first-question").await;
    finished_turn(&mut rig, "tool-slow second-question").await;
    let parent = rig.session.clone();
    let answer = fork(&mut rig, json!({})).await;
    rewind_parent_to_first_turn(&mut rig).await;
    rig.session = SessionId::new(answer["result"]["session"]["id"].as_str().unwrap());

    // Act
    finished_turn(&mut rig, "tool-slow fork-question").await;

    // Assert
    let prompts = read(&rig, PROMPT_LOG).unwrap();
    let last = prompts.lines().last().unwrap();
    assert!(last.contains("second-question"), "{last}");
    assert_ne!(rig.session, parent);
}

#[tokio::test]
async fn the_default_fork_point_skips_turns_that_are_rolled_back() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    let first = first_turn_of(&rig).await;
    rewind_parent_to_first_turn(&mut rig).await;

    // Act
    let answer = fork(&mut rig, json!({})).await;

    // Assert
    assert_eq!(answer["result"]["source_turn"], first, "{answer}");
}

#[tokio::test]
async fn a_rolled_back_turn_cannot_be_named_as_the_fork_point() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    let id = rig.session.clone();
    let turns = rig
        .hub
        .with_store(move |store| store.turn_checkpoints(&id))
        .await
        .unwrap();
    let second = json!(turns[1].turn);
    rewind_parent_to_first_turn(&mut rig).await;

    // Act
    let answer = fork(&mut rig, json!({"turn_id": second})).await;

    // Assert
    assert_eq!(answer["error"]["code"], INVALID_PARAMS, "{answer}");
}

#[tokio::test]
async fn a_turn_of_another_session_cannot_be_a_fork_point_of_this_one() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    let foreign = rig
        .hub
        .with_store_mut(|store| {
            let other = store.create_session(&coda_store::NewSession {
                backend: BackendKind::ClaudeCode,
                model: None,
                cwd: "/".to_owned(),
                mode: coda_protocol::types::Mode::Default,
            })?;
            store.begin_turn(&other)
        })
        .await
        .unwrap();

    // Act
    let answer = fork(&mut rig, json!({"turn_id": foreign})).await;

    // Assert
    assert_eq!(answer["error"]["code"], INVALID_PARAMS, "{answer}");
}

#[tokio::test]
async fn a_session_with_a_turn_running_cannot_be_forked() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    rig.send("hello").await;
    rig.wait_for_pending().await;

    // Act
    let answer = fork(&mut rig, json!({})).await;

    // Assert
    assert_eq!(answer["error"]["code"], super::TURN_RUNNING, "{answer}");
}
