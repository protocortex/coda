// SPDX-License-Identifier: Apache-2.0

//! `session.resume_at_reset`: a turn that ends at the plan limit is continued at the reset.

use std::time::Duration;

use coda_protocol::jsonrpc::INVALID_PARAMS;
use coda_store::NewQueued;
use serde_json::{Value, json};

use super::{POLL, Rig, SESSION_NOT_FOUND, WAIT, relaxed};
use crate::turn::reset::{RESUME_PROMPT, ResetPolicy};

/// The reset time `fake-claude` reports when a turn hits the limit.
const FAKE_RESET_AT: u64 = 1_794_193_200;
/// Long enough for a resume that should not happen to show itself.
const SETTLE: Duration = Duration::from_millis(600);
const LIMIT_TRIGGER: &str = "hit-limit";

fn a_second_before_reset() -> u64 {
    FAKE_RESET_AT - 1
}

fn an_hour_before_reset() -> u64 {
    FAKE_RESET_AT - 3_600
}

fn after_reset() -> u64 {
    FAKE_RESET_AT + 1
}

/// A clock an hour short of the reset, so a waiting resume stays waiting.
fn far_policy() -> ResetPolicy {
    ResetPolicy {
        now: an_hour_before_reset,
        margin: Duration::ZERO,
    }
}

fn quick_policy() -> ResetPolicy {
    ResetPolicy {
        now: a_second_before_reset,
        margin: Duration::from_millis(50),
    }
}

async fn opt_in(rig: &mut Rig, enabled: bool) -> Value {
    let params = json!({"session_id": rig.session, "enabled": enabled});
    rig.call("session.resume_at_reset", params).await
}

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
    .expect("the message did not arrive in time");
}

async fn hit_the_limit(rig: &mut Rig) {
    rig.send(LIMIT_TRIGGER).await;
    rig.wait_for("usage_limit").await;
    rig.wait_until_idle().await;
}

#[tokio::test]
async fn an_opted_in_session_continues_when_the_window_resets() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.turns.set_reset_policy(quick_policy());
    opt_in(&mut rig, true).await;

    // Act
    hit_the_limit(&mut rig).await;
    wait_for_user_messages(&rig, 2).await;

    // Assert
    assert_eq!(user_messages(&rig).await, [LIMIT_TRIGGER, RESUME_PROMPT]);
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_until_idle().await;
}

#[tokio::test]
async fn a_session_that_did_not_opt_in_is_left_stopped() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.turns.set_reset_policy(quick_policy());

    // Act
    hit_the_limit(&mut rig).await;
    tokio::time::sleep(SETTLE).await;

    // Assert
    assert_eq!(user_messages(&rig).await, [LIMIT_TRIGGER]);
    assert!(!rig.turns.resume_scheduled(&rig.session));
}

#[tokio::test]
async fn opting_out_cancels_a_resume_that_is_waiting() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.turns.set_reset_policy(far_policy());
    opt_in(&mut rig, true).await;
    hit_the_limit(&mut rig).await;
    assert!(rig.turns.resume_scheduled(&rig.session));

    // Act
    opt_in(&mut rig, false).await;

    // Assert
    assert!(!rig.turns.resume_scheduled(&rig.session));
}

#[tokio::test]
async fn a_new_message_cancels_the_waiting_resume() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.turns.set_reset_policy(far_policy());
    opt_in(&mut rig, true).await;
    hit_the_limit(&mut rig).await;
    assert!(rig.turns.resume_scheduled(&rig.session));

    // Act
    rig.send("something else").await;

    // Assert
    assert!(!rig.turns.resume_scheduled(&rig.session));
}

#[tokio::test]
async fn a_stop_cancels_the_waiting_resume() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.turns.set_reset_policy(far_policy());
    opt_in(&mut rig, true).await;
    hit_the_limit(&mut rig).await;

    // Act
    rig.call("session.cancel", json!({"session_id": rig.session}))
        .await;

    // Assert
    assert!(!rig.turns.resume_scheduled(&rig.session));
}

#[tokio::test]
async fn a_forced_stop_cancels_the_waiting_resume() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.turns.set_reset_policy(far_policy());
    opt_in(&mut rig, true).await;
    hit_the_limit(&mut rig).await;

    // Act
    rig.call(
        "session.cancel",
        json!({"session_id": rig.session, "force": true}),
    )
    .await;

    // Assert
    assert!(!rig.turns.resume_scheduled(&rig.session));
}

#[tokio::test]
async fn a_queue_resume_that_starts_a_turn_cancels_the_waiting_resume() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.turns.set_reset_policy(far_policy());
    opt_in(&mut rig, true).await;
    hit_the_limit(&mut rig).await;
    let id = rig.session.clone();
    rig.hub
        .with_store_mut(move |store| {
            store.enqueue_message(
                &id,
                NewQueued {
                    text: "later",
                    max_turns: None,
                    token_cap: None,
                },
            )
        })
        .await
        .unwrap();

    // Act
    rig.call(
        "session.queue",
        json!({"session_id": rig.session, "action": "resume"}),
    )
    .await;

    // Assert
    assert!(!rig.turns.resume_scheduled(&rig.session));
}

#[tokio::test]
async fn a_send_that_is_refused_keeps_the_wait() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.turns.set_reset_policy(far_policy());
    opt_in(&mut rig, true).await;
    hit_the_limit(&mut rig).await;

    // Act
    let refused = rig
        .call(
            "session.send",
            json!({"session_id": "no-such-session", "text": "x"}),
        )
        .await;

    // Assert
    assert!(refused["error"].is_object(), "{refused}");
    assert!(rig.turns.resume_scheduled(&rig.session));
}

#[tokio::test]
async fn a_reset_time_that_has_passed_schedules_nothing() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.turns.set_reset_policy(ResetPolicy {
        now: after_reset,
        margin: Duration::ZERO,
    });
    opt_in(&mut rig, true).await;

    // Act
    hit_the_limit(&mut rig).await;

    // Assert
    assert!(!rig.turns.resume_scheduled(&rig.session));
}

#[tokio::test]
async fn closing_a_session_forgets_its_opt_in_and_wait() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.turns.set_reset_policy(far_policy());
    opt_in(&mut rig, true).await;
    hit_the_limit(&mut rig).await;

    // Act
    rig.call("session.close", json!({"session_id": rig.session}))
        .await;

    // Assert
    assert!(!rig.turns.resume_scheduled(&rig.session));
    assert!(!rig.turns.resume_opted_in(&rig.session));
}

#[tokio::test]
async fn the_same_reset_of_the_same_turn_resumes_only_once() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.turns.set_reset_policy(far_policy());
    opt_in(&mut rig, true).await;
    hit_the_limit(&mut rig).await;
    let turn = rig
        .events()
        .await
        .into_iter()
        .find(|e| e.kind == "usage_limit")
        .map(|e| e.turn)
        .expect("the limit event has a turn");

    // Act
    let first = rig
        .turns
        .resume_now(&rig.session, &turn, FAKE_RESET_AT)
        .await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_until_idle().await;
    let repeat = rig
        .turns
        .resume_now(&rig.session, &turn, FAKE_RESET_AT)
        .await;

    // Assert
    assert!(matches!(first, Ok(Some(_))), "{first:?}");
    assert!(matches!(repeat, Ok(None)), "{repeat:?}");
    assert_eq!(user_messages(&rig).await, [LIMIT_TRIGGER, RESUME_PROMPT]);
}

#[tokio::test]
async fn the_method_reports_the_setting_and_refuses_an_unknown_session() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let on = opt_in(&mut rig, true).await;
    let off = opt_in(&mut rig, false).await;
    let missing = rig
        .call(
            "session.resume_at_reset",
            json!({"session_id": "no-such-session", "enabled": true}),
        )
        .await;
    let malformed = rig
        .call(
            "session.resume_at_reset",
            json!({"session_id": rig.session}),
        )
        .await;

    // Assert
    assert_eq!(on["result"]["enabled"], true, "{on}");
    assert_eq!(off["result"]["enabled"], false, "{off}");
    assert_eq!(missing["error"]["code"], SESSION_NOT_FOUND, "{missing}");
    assert_eq!(malformed["error"]["code"], INVALID_PARAMS, "{malformed}");
}
