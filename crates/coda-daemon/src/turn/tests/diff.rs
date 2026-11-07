// SPDX-License-Identifier: Apache-2.0

//! `session.diff` reports what one turn changed, from the snapshots around it.

use coda_protocol::methods::{DIFF_REFUSED_CODE, TURN_RUNNING_CODE};
use coda_protocol::types::TurnId;
use serde_json::{Value, json};

use super::checkpoints::{checkpoints, git, repo_rig};
use super::{Rig, relaxed};

const NO_PERMISSION_TURN: &str = "tool-slow";

async fn finished_turn(rig: &mut Rig, text: &str) {
    rig.send(text).await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
}

async fn diff(rig: &mut Rig, mut params: Value) -> Value {
    let session = rig.session.clone();
    params
        .as_object_mut()
        .unwrap()
        .entry("session_id")
        .or_insert_with(|| json!(session));
    rig.call("session.diff", params).await
}

async fn two_turns() -> (Rig, TurnId, TurnId) {
    let mut rig = repo_rig();
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    std::fs::write(rig.cwd.path().join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(rig.cwd.path().join("b.txt"), "new\n").unwrap();
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;
    let found = checkpoints(&rig).await;
    (rig, found[0].turn.clone(), found[1].turn.clone())
}

fn paths(answer: &Value) -> Vec<String> {
    answer["result"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn with_no_turn_named_it_reports_the_latest_turn_against_the_one_before() {
    // Arrange
    let (mut rig, _first, second) = two_turns().await;

    // Act
    let answer = diff(&mut rig, json!({})).await;

    // Assert
    assert_eq!(answer["result"]["turn_id"], json!(second));
    assert_eq!(paths(&answer), ["a.txt", "b.txt"]);
    assert_eq!(answer["result"]["files"][0]["added"], 1);
    assert_eq!(answer["result"].get("patch"), None);
}

#[tokio::test]
async fn the_first_turn_is_compared_with_the_baseline_taken_before_it() {
    // Arrange
    let (mut rig, first, _second) = two_turns().await;

    // Act
    let answer = diff(&mut rig, json!({"turn_id": first, "patch": true})).await;

    // Assert
    assert_eq!(answer["result"]["turn_id"], json!(first));
    assert_eq!(paths(&answer), Vec::<String>::new());
    assert_eq!(answer["result"].get("patch"), None);
}

#[tokio::test]
async fn asking_for_the_patch_returns_the_unified_diff() {
    // Arrange
    let (mut rig, _first, _second) = two_turns().await;

    // Act
    let answer = diff(&mut rig, json!({"patch": true})).await;

    // Assert
    let patch = answer["result"]["patch"].as_str().unwrap();
    assert!(patch.starts_with("diff --git a/a.txt b/a.txt\n"), "{patch}");
    assert!(patch.contains("+two\n"), "{patch}");
    assert!(patch.contains("diff --git a/b.txt b/b.txt\n"), "{patch}");
}

#[tokio::test]
async fn the_users_index_and_head_are_not_touched() {
    // Arrange
    let (mut rig, _first, _second) = two_turns().await;
    git(&rig, &["add", "a.txt"]);
    let head = git(&rig, &["rev-parse", "HEAD"]);
    let status = git(&rig, &["status", "--porcelain"]);

    // Act
    let _ = diff(&mut rig, json!({"patch": true})).await;

    // Assert
    assert_eq!(git(&rig, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&rig, &["status", "--porcelain"]), status);
}

#[tokio::test]
async fn a_turn_after_a_rewind_is_compared_with_the_turn_the_files_went_back_to() {
    // Arrange
    let (mut rig, first, _second) = two_turns().await;
    rig.call(
        "session.rewind",
        json!({"session_id": rig.session, "turn_id": first}),
    )
    .await;
    std::fs::write(rig.cwd.path().join("c.txt"), "later\n").unwrap();
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;

    // Act
    let answer = diff(&mut rig, json!({})).await;

    // Assert
    assert_eq!(paths(&answer), ["c.txt"]);
}

#[tokio::test]
async fn a_rolled_back_or_unknown_turn_is_refused() {
    // Arrange
    let (mut rig, first, second) = two_turns().await;
    rig.call(
        "session.rewind",
        json!({"session_id": rig.session, "turn_id": first}),
    )
    .await;

    // Act
    let rolled_back = diff(&mut rig, json!({"turn_id": second})).await;
    let unknown = diff(&mut rig, json!({"turn_id": "999"})).await;

    // Assert
    assert_eq!(rolled_back["error"]["code"], DIFF_REFUSED_CODE);
    assert_eq!(unknown["error"]["code"], DIFF_REFUSED_CODE);
}

#[tokio::test]
async fn a_session_with_no_snapshot_yet_is_refused() {
    // Arrange
    let mut rig = repo_rig();

    // Act
    let answer = diff(&mut rig, json!({})).await;

    // Assert
    assert_eq!(answer["error"]["code"], DIFF_REFUSED_CODE);
}

#[tokio::test]
async fn a_folder_that_is_not_a_repository_has_no_diff() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    finished_turn(&mut rig, NO_PERMISSION_TURN).await;

    // Act
    let answer = diff(&mut rig, json!({})).await;

    // Assert
    assert_eq!(answer["error"]["code"], DIFF_REFUSED_CODE);
}

#[tokio::test]
async fn a_session_that_does_not_exist_is_not_found() {
    // Arrange
    let mut rig = repo_rig();

    // Act
    let answer = diff(&mut rig, json!({"session_id": "nope"})).await;

    // Assert
    assert_eq!(answer["error"]["code"], super::SESSION_NOT_FOUND);
}

#[tokio::test]
async fn a_live_turn_makes_the_request_wait_no_longer_than_to_refuse_it() {
    // Arrange
    let (mut rig, _first, _second) = two_turns().await;
    rig.send("hang").await;

    // Act
    let answer = diff(&mut rig, json!({})).await;

    // Assert
    assert_eq!(answer["error"]["code"], TURN_RUNNING_CODE);
}
