// SPDX-License-Identifier: Apache-2.0

//! `session.rewind` restores the files and rolls the conversation back.

use coda_protocol::methods::{REWIND_REFUSED_CODE, TURN_RUNNING_CODE};
use coda_protocol::types::TurnId;
use serde_json::{Value, json};

use super::checkpoints::{checkpoints, git, repo_rig};
use super::{Rig, relaxed};

const NO_PERMISSION_TURN: &str = "tool-slow";
const PROMPT_LOG: &str = "prompts.log";
const RESUME_FLAG: &str = "--resume";
const REPORT: &str = "report.json";

async fn finished_turn(rig: &mut Rig, text: &str) {
    rig.send(text).await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
}

async fn two_turns() -> (Rig, TurnId, TurnId) {
    let mut rig = repo_rig();
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    std::fs::write(rig.cwd.path().join("a.txt"), "edited\n").unwrap();
    std::fs::write(rig.cwd.path().join("b.txt"), "new\n").unwrap();
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    let found = checkpoints(&rig).await;
    (rig, found[0].turn.clone(), found[1].turn.clone())
}

/// Rewinds the rig's session unless `params` names another one.
async fn rewind(rig: &mut Rig, mut params: Value) -> Value {
    let session = rig.session.clone();
    params
        .as_object_mut()
        .unwrap()
        .entry("session_id")
        .or_insert_with(|| json!(session));
    rig.call("session.rewind", params).await
}

fn read(rig: &Rig, name: &str) -> Option<String> {
    std::fs::read_to_string(rig.cwd.path().join(name)).ok()
}

async fn wait_for_pending_of(rig: &Rig, session: &coda_protocol::types::SessionId) {
    tokio::time::timeout(super::WAIT, async {
        loop {
            let id = session.clone();
            let open = rig
                .hub
                .with_store(move |store| store.list_pending(&id))
                .await
                .unwrap();
            if !open.is_empty() {
                return;
            }
            tokio::time::sleep(super::POLL).await;
        }
    })
    .await
    .expect("the other session never asked for a permission");
}

async fn user_messages(rig: &Rig) -> usize {
    rig.kinds()
        .await
        .iter()
        .filter(|k| *k == "user_message")
        .count()
}

#[tokio::test]
async fn rewinding_to_a_turn_restores_its_files_and_rolls_back_the_later_turns() {
    // Arrange
    let (mut rig, first, _second) = two_turns().await;
    let head = git(&rig, &["rev-parse", "HEAD"]);

    // Act
    let answer = rewind(&mut rig, json!({"turn_id": first})).await;

    // Assert
    assert_eq!(answer["result"]["files_restored"], true);
    assert_eq!(answer["result"]["turns_rolled_back"], 1);
    assert_eq!(read(&rig, "a.txt").as_deref(), Some("one\n"));
    assert_eq!(read(&rig, "b.txt"), None);
    assert_eq!(git(&rig, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&rig, &["diff", "--cached", "--name-only"]), "");
    assert_eq!(
        user_messages(&rig).await,
        2,
        "rolled back turns stay stored"
    );
}

#[tokio::test]
async fn rewinding_with_no_turn_goes_back_to_before_the_first_turn() {
    // Arrange
    let (mut rig, _first, _second) = two_turns().await;
    std::fs::write(rig.cwd.path().join("late.txt"), "late\n").unwrap();

    // Act
    let answer = rewind(&mut rig, json!({})).await;

    // Assert
    assert_eq!(answer["result"]["turns_rolled_back"], 2);
    assert_eq!(read(&rig, "a.txt").as_deref(), Some("one\n"));
    assert_eq!(read(&rig, "b.txt"), None);
    assert_eq!(read(&rig, "late.txt"), None);
}

#[tokio::test]
async fn keeping_the_files_rolls_back_the_conversation_only() {
    // Arrange
    let (mut rig, first, _second) = two_turns().await;

    // Act
    let answer = rewind(&mut rig, json!({"turn_id": first, "keep_files": true})).await;

    // Assert
    assert_eq!(answer["result"]["files_restored"], false);
    assert_eq!(answer["result"]["turns_rolled_back"], 1);
    assert_eq!(answer["result"].get("saved_as"), None);
    assert_eq!(read(&rig, "a.txt").as_deref(), Some("edited\n"));
}

#[tokio::test]
async fn keeping_the_conversation_restores_the_files_only() {
    // Arrange
    let (mut rig, first, _second) = two_turns().await;

    // Act
    let answer = rewind(
        &mut rig,
        json!({"turn_id": first, "keep_conversation": true}),
    )
    .await;

    // Assert
    assert_eq!(answer["result"]["files_restored"], true);
    assert_eq!(answer["result"]["turns_rolled_back"], 0);
    assert_eq!(read(&rig, "a.txt").as_deref(), Some("one\n"));
}

#[tokio::test]
async fn a_running_turn_refuses_the_rewind() {
    // Arrange
    let mut rig = repo_rig();
    rig.send("hello").await;
    rig.wait_for_pending().await;

    // Act
    let answer = rewind(&mut rig, json!({})).await;

    // Assert
    assert_eq!(answer["error"]["code"], TURN_RUNNING_CODE);
}

#[tokio::test]
async fn another_running_session_in_the_folder_refuses_the_rewind() {
    // Arrange
    let (mut rig, first, _second) = two_turns().await;
    let other = rig.other_session().await;
    rig.call(
        "session.send",
        json!({"session_id": other, "text": "hello"}),
    )
    .await;
    wait_for_pending_of(&rig, &other).await;

    // Act
    let answer = rewind(&mut rig, json!({"turn_id": first})).await;

    // Assert
    assert_eq!(answer["error"]["code"], REWIND_REFUSED_CODE);
    assert_eq!(read(&rig, "a.txt").as_deref(), Some("edited\n"));
    assert_eq!(user_messages(&rig).await, 2);
}

#[tokio::test]
async fn a_held_index_lock_refuses_the_rewind_and_changes_nothing() {
    // Arrange
    let (mut rig, first, _second) = two_turns().await;
    std::fs::write(rig.cwd.path().join(".git/index.lock"), "").unwrap();

    // Act
    let answer = rewind(&mut rig, json!({"turn_id": first})).await;

    // Assert
    assert_eq!(answer["error"]["code"], REWIND_REFUSED_CODE);
    assert_eq!(read(&rig, "a.txt").as_deref(), Some("edited\n"));
    let marked = rig
        .hub
        .with_store({
            let session = rig.session.clone();
            move |store| store.rolled_back_turns(&session)
        })
        .await
        .unwrap();
    assert!(marked.is_empty());
}

#[tokio::test]
async fn a_turn_with_no_snapshot_is_refused() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    let turn = checkpoints(&rig).await[0].turn.clone();

    // Act
    let answer = rewind(&mut rig, json!({"turn_id": turn})).await;

    // Assert
    assert_eq!(answer["error"]["code"], REWIND_REFUSED_CODE);
}

#[tokio::test]
async fn an_unknown_turn_is_refused() {
    // Arrange
    let (mut rig, _first, _second) = two_turns().await;

    // Act
    let answer = rewind(&mut rig, json!({"turn_id": "999"})).await;

    // Assert
    assert_eq!(answer["error"]["code"], REWIND_REFUSED_CODE);
    assert_eq!(read(&rig, "a.txt").as_deref(), Some("edited\n"));
}

#[tokio::test]
async fn a_turn_already_rolled_back_is_refused() {
    // Arrange
    let (mut rig, first, second) = two_turns().await;
    rewind(&mut rig, json!({"turn_id": first})).await;

    // Act
    let answer = rewind(&mut rig, json!({"turn_id": second})).await;

    // Assert
    assert_eq!(answer["error"]["code"], REWIND_REFUSED_CODE);
}

#[tokio::test]
async fn an_unknown_session_is_not_found() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let answer = rewind(&mut rig, json!({"session_id": "999"})).await;

    // Assert
    assert_eq!(answer["error"]["code"], super::SESSION_NOT_FOUND);
}

#[tokio::test]
async fn the_next_claude_turn_starts_a_fresh_conversation_told_only_the_kept_turns() {
    // Arrange
    let mut rig = repo_rig();
    rig.control(&json!({
        "report_path": rig.cwd.path().join(REPORT),
        "prompt_path": rig.cwd.path().join(PROMPT_LOG),
    }));
    finished_turn(&mut rig, "tool-slow kept-question").await;
    finished_turn(&mut rig, "tool-slow dropped-question").await;
    let first = checkpoints(&rig).await[0].turn.clone();
    rewind(&mut rig, json!({"turn_id": first, "keep_files": true})).await;

    // Act
    finished_turn(&mut rig, "tool-slow next-question").await;

    // Assert
    let prompts = read(&rig, PROMPT_LOG).unwrap();
    let last = prompts.lines().last().unwrap();
    assert!(last.contains("next-question"), "{last}");
    assert!(last.contains("kept-question"), "{last}");
    assert!(!last.contains("dropped-question"), "{last}");
    let report: Value = serde_json::from_str(&read(&rig, REPORT).unwrap()).unwrap();
    assert!(
        !report["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == RESUME_FLAG),
        "{report}"
    );
}

#[test]
fn every_error_code_the_daemon_answers_with_is_distinct() {
    // Arrange
    let codes = [
        crate::dispatch::PROTOCOL_VERSION_MISMATCH,
        super::SESSION_NOT_FOUND,
        super::TURN_RUNNING,
        super::BACKEND_UNSUPPORTED,
        crate::registry::UNKNOWN_MODEL,
        crate::registry::MODEL_UNAVAILABLE,
        REWIND_REFUSED_CODE,
    ];

    // Act
    let distinct: std::collections::HashSet<_> = codes.iter().collect();

    // Assert
    assert_eq!(distinct.len(), codes.len(), "{codes:?}");
}

#[tokio::test]
async fn rewinding_to_the_latest_turn_keeps_resuming_the_claude_conversation() {
    // Arrange
    let mut rig = repo_rig();
    rig.control(&json!({"report_path": rig.cwd.path().join(REPORT)}));
    finished_turn(&mut rig, "tool-slow first-question").await;
    finished_turn(&mut rig, "tool-slow second-question").await;
    let latest = checkpoints(&rig).await[1].turn.clone();

    // Act
    let answer = rewind(&mut rig, json!({"turn_id": latest})).await;
    finished_turn(&mut rig, "tool-slow third-question").await;

    // Assert
    assert_eq!(answer["result"]["turns_rolled_back"], 0);
    let report: Value = serde_json::from_str(&read(&rig, REPORT).unwrap()).unwrap();
    let resumed = report["args"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a == RESUME_FLAG);
    assert!(
        resumed,
        "nothing was rolled back, so the conversation stays: {report}"
    );
}

#[tokio::test]
async fn the_files_as_they_were_before_the_restore_are_saved_in_a_private_ref() {
    // Arrange
    let (mut rig, first, _second) = two_turns().await;
    std::fs::write(rig.cwd.path().join("a.txt"), "my own edit\n").unwrap();
    std::fs::write(rig.cwd.path().join("mine.txt"), "mine\n").unwrap();

    // Act
    let answer = rewind(&mut rig, json!({"turn_id": first})).await;

    // Assert
    let saved = answer["result"]["saved_as"].as_str().unwrap().to_owned();
    assert!(saved.starts_with("refs/coda/checkpoints/"), "{saved}");
    assert_eq!(read(&rig, "mine.txt"), None);
    assert_eq!(git(&rig, &["show", &format!("{saved}:mine.txt")]), "mine\n");
    assert_eq!(
        git(&rig, &["show", &format!("{saved}:a.txt")]),
        "my own edit\n"
    );
}

#[tokio::test]
async fn a_session_in_a_folder_inside_or_above_this_one_refuses_the_rewind() {
    // Arrange
    let (mut rig, first, _second) = two_turns().await;
    let inside = rig.cwd.path().join("inside");
    std::fs::create_dir(&inside).unwrap();
    let other = rig
        .hub
        .with_store_mut({
            let cwd = inside.to_string_lossy().into_owned();
            move |store| {
                store.create_session(&coda_store::NewSession {
                    backend: coda_protocol::types::BackendKind::ClaudeCode,
                    model: None,
                    cwd,
                    mode: coda_protocol::types::Mode::Default,
                })
            }
        })
        .await
        .unwrap();
    rig.call(
        "session.send",
        json!({"session_id": other, "text": "hello"}),
    )
    .await;
    wait_for_pending_of(&rig, &other).await;

    // Act
    let answer = rewind(&mut rig, json!({"turn_id": first})).await;

    // Assert
    assert_eq!(answer["error"]["code"], REWIND_REFUSED_CODE);
    assert_eq!(read(&rig, "a.txt").as_deref(), Some("edited\n"));
}

#[tokio::test]
async fn a_conversation_that_cannot_be_forgotten_refuses_before_any_turn_is_marked() {
    // Arrange
    let (mut rig, first, _second) = two_turns().await;
    rig.db()
        .execute_batch("DROP TABLE backend_threads")
        .unwrap();

    // Act
    let answer = rewind(&mut rig, json!({"turn_id": first, "keep_files": true})).await;

    // Assert
    assert!(answer["error"].is_object(), "{answer}");
    let session = rig.session.clone();
    let marked = rig
        .hub
        .with_store(move |store| store.rolled_back_turns(&session))
        .await
        .unwrap();
    assert!(marked.is_empty());
}

#[tokio::test]
async fn going_back_to_base_is_refused_when_the_first_turn_has_no_baseline() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    git(&rig, &["init", "-q", "-b", "main"]);
    std::fs::write(rig.cwd.path().join("a.txt"), "one\n").unwrap();
    git(&rig, &["add", "."]);
    git(&rig, &["commit", "-q", "-m", "first"]);
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;

    // Act
    let answer = rewind(&mut rig, json!({})).await;

    // Assert
    assert_eq!(answer["error"]["code"], REWIND_REFUSED_CODE);
}

#[tokio::test]
async fn keeping_both_the_files_and_the_conversation_is_refused() {
    // Arrange
    let (mut rig, first, _second) = two_turns().await;

    // Act
    let answer = rewind(
        &mut rig,
        json!({"turn_id": first, "keep_files": true, "keep_conversation": true}),
    )
    .await;

    // Assert
    assert_eq!(answer["error"]["code"], REWIND_REFUSED_CODE);
}
