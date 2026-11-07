// SPDX-License-Identifier: Apache-2.0

//! A scheduled prompt sent through the daemon's turns.

use coda_protocol::methods::{ScheduleInfo, ScheduleSpec, ScheduleState, ScheduleTarget};
use coda_protocol::types::{BackendKind, Mode};
use serde_json::Value;

use super::{POLL, Rig, WAIT, relaxed};
use crate::scheduler::{RunTarget, TurnsRunner};

const PROMPT: &str = "check the build";

fn schedule(target: ScheduleTarget) -> ScheduleInfo {
    ScheduleInfo {
        id: 1,
        spec: ScheduleSpec::Interval { every_secs: 60 },
        target,
        text: PROMPT.to_owned(),
        state: ScheduleState::Running,
        next_run_at: None,
        last_run_at: None,
        runs: 1,
        created_at: 0,
    }
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

#[tokio::test]
async fn a_run_into_a_session_sends_the_prompt_as_a_user_message() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let runner = TurnsRunner::new(rig.hub.clone(), rig.turns.clone());
    let target = ScheduleTarget::Session {
        session_id: rig.session.clone(),
    };

    // Act
    let sent = runner.run(&schedule(target)).await;

    // Assert
    assert_eq!(sent, Ok(()));
    tokio::time::timeout(WAIT, async {
        while user_messages(&rig).await.is_empty() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the prompt did not start in time");
    assert_eq!(user_messages(&rig).await, [PROMPT]);
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_until_idle().await;
}

#[tokio::test]
async fn a_run_into_a_new_session_makes_the_session_first() {
    // Arrange
    let rig = Rig::new(relaxed(), Vec::new());
    let runner = TurnsRunner::new(rig.hub.clone(), rig.turns.clone());
    let target = ScheduleTarget::NewSession {
        backend: BackendKind::ClaudeCode,
        model: None,
        cwd: rig.cwd.path().to_string_lossy().into_owned(),
        mode: Mode::Default,
    };
    let before = rig
        .hub
        .with_store(|store| store.list_sessions(&coda_store::SessionFilter::default()))
        .await
        .unwrap()
        .len();

    // Act
    let sent = runner.run(&schedule(target)).await;

    // Assert
    assert_eq!(sent, Ok(()));
    let after = rig
        .hub
        .with_store(|store| store.list_sessions(&coda_store::SessionFilter::default()))
        .await
        .unwrap();
    assert_eq!(after.len(), before + 1);
    let made = after.last().unwrap().id.clone();
    rig.turns.cancel(&made);
    tokio::time::timeout(WAIT, async {
        while rig.turns.is_running(&made) {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the new session's turn did not end in time");
}

#[tokio::test]
async fn a_run_into_a_session_that_does_not_exist_says_so() {
    // Arrange
    let rig = Rig::new(relaxed(), Vec::new());
    let runner = TurnsRunner::new(rig.hub.clone(), rig.turns.clone());
    let target = ScheduleTarget::Session {
        session_id: coda_protocol::types::SessionId::new("999"),
    };

    // Act
    let sent = runner.run(&schedule(target)).await;

    // Assert
    assert!(sent.is_err(), "{sent:?}");
}
