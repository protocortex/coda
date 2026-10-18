// SPDX-License-Identifier: AGPL-3.0-only

//! The real `coda daemon` process driven through the client: create a session,
//! send a message, answer the permission prompt and read the result.

mod common;

use coda_protocol::methods::{
    DAEMON_STATUS, DaemonStatusParams, DaemonStatusResult, PERMISSION_RESPOND,
    PermissionRespondParams, PermissionRespondResult, SESSION_CREATE, SESSION_RESUME, SESSION_SEND,
    SessionCreateParams, SessionCreateResult, SessionResumeParams, SessionResumeResult,
    SessionSendParams, SessionSendResult,
};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision, RequestId};
use common::{connect, next_matching, short_dir, start_daemon};

const PERMISSION_NOTIFICATION: &str = "permission.request";

#[tokio::test]
async fn a_session_runs_a_turn_with_a_permission_prompt_through_the_real_daemon() {
    // Arrange
    let home = short_dir();
    let work = short_dir();
    let _daemon = start_daemon(home.path());
    let client = connect(home.path()).await;

    // Act
    let created: SessionCreateResult = client
        .request(
            SESSION_CREATE,
            &SessionCreateParams {
                backend: BackendKind::ClaudeCode,
                cwd: work.path().display().to_string(),
                model: None,
                mode: Mode::Default,
            },
        )
        .await
        .unwrap();
    let session = created.session.id.clone();
    let mut events = client.subscribe(&session, Some(0)).await.unwrap();
    let resumed: SessionResumeResult = client
        .request(
            SESSION_RESUME,
            &SessionResumeParams {
                session_id: session.clone(),
            },
        )
        .await
        .unwrap();
    let sent: SessionSendResult = client
        .request(
            SESSION_SEND,
            &SessionSendParams {
                session_id: session.clone(),
                text: "hello".to_owned(),
                max_turns: None,
                token_cap: None,
            },
        )
        .await
        .unwrap();
    let prompt = next_matching(&mut events, |method, _| method == PERMISSION_NOTIFICATION).await;
    let request_id = RequestId::new(prompt["request_id"].as_str().unwrap());
    let busy: DaemonStatusResult = client
        .request(DAEMON_STATUS, &DaemonStatusParams {})
        .await
        .unwrap();
    let answered: PermissionRespondResult = client
        .request(
            PERMISSION_RESPOND,
            &PermissionRespondParams {
                session_id: session.clone(),
                request_id,
                decision: PermissionDecision::AllowOnce,
            },
        )
        .await
        .unwrap();
    let result = next_matching(&mut events, |method, params| {
        method == "session.event" && params["kind"] == "result"
    })
    .await;

    // Assert
    assert_eq!(resumed.session, created.session);
    assert_ne!(sent.turn_id.as_str(), "");
    assert_eq!(busy.active_turns, 1);
    assert!(answered.accepted);
    assert_eq!(result["session_id"], session.as_str());
}
