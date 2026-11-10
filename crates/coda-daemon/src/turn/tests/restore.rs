// SPDX-License-Identifier: Apache-2.0

//! Every send the daemon starts on its own puts back a cleaned worktree first.

use coda_store::NewQueued;
use serde_json::Value;

use super::schedule::{cleaned_worktree, session_in_worktree};
use super::{POLL, Rig, WAIT, relaxed};
use crate::turn::reset::RESUME_PROMPT;

const WAITING: &str = "carry on";
const LIMIT_TRIGGER: &str = "hit-limit";
const FAKE_RESET_AT: u64 = 1_794_193_200;

async fn user_messages(rig: &Rig) -> Vec<String> {
    rig.events()
        .await
        .into_iter()
        .filter(|e| e.kind == "user_message")
        .map(|e| {
            serde_json::from_str::<Value>(&e.body).unwrap()["text"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

async fn wait_for_user_messages(rig: &Rig, count: usize) {
    tokio::time::timeout(WAIT, async {
        while user_messages(rig).await.len() < count {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the message did not start in time");
}

async fn queue_waiting_message(rig: &Rig) {
    let id = rig.session.clone();
    rig.hub
        .with_store_mut(move |store| {
            store.enqueue_message(
                &id,
                NewQueued {
                    text: WAITING,
                    max_turns: None,
                    token_cap: None,
                },
            )
        })
        .await
        .unwrap();
}

async fn finish_the_turn(rig: &mut Rig) {
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_until_idle().await;
}

#[tokio::test]
async fn a_queued_message_that_starts_after_a_turn_restores_a_cleaned_worktree() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let path = cleaned_worktree(&mut rig).await;
    queue_waiting_message(&rig).await;

    // Act
    rig.turns.clone().start_next_queued(rig.session.clone());

    // Assert
    wait_for_user_messages(&rig, 1).await;
    assert!(path.join("a.txt").exists());
    assert_eq!(user_messages(&rig).await, [WAITING]);
    finish_the_turn(&mut rig).await;
}

#[tokio::test]
async fn a_restart_continuation_restores_a_cleaned_worktree() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let path = cleaned_worktree(&mut rig).await;
    queue_waiting_message(&rig).await;

    // Act
    rig.turns
        .start_continuations(std::slice::from_ref(&rig.session));

    // Assert
    wait_for_user_messages(&rig, 1).await;
    assert!(path.join("a.txt").exists());
    assert_eq!(user_messages(&rig).await, [WAITING]);
    finish_the_turn(&mut rig).await;
}

#[tokio::test]
async fn a_resume_at_reset_restores_a_cleaned_worktree() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let path = session_in_worktree(&mut rig).await;
    rig.send(LIMIT_TRIGGER).await;
    rig.wait_for("usage_limit").await;
    rig.wait_until_idle().await;
    let turn = rig
        .events()
        .await
        .into_iter()
        .find(|e| e.kind == "usage_limit")
        .map(|e| e.turn)
        .expect("the limit event has a turn");
    std::fs::remove_dir_all(&path).unwrap();

    // Act
    let resumed = rig
        .turns
        .resume_now(&rig.session, &turn, FAKE_RESET_AT)
        .await;

    // Assert
    assert!(matches!(resumed, Ok(Some(_))), "{resumed:?}");
    assert!(path.join("a.txt").exists());
    assert_eq!(user_messages(&rig).await, [LIMIT_TRIGGER, RESUME_PROMPT]);
    finish_the_turn(&mut rig).await;
}
