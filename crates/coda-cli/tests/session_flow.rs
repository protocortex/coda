// SPDX-License-Identifier: AGPL-3.0-only

//! The real `coda daemon` process driven through the client: create a session,
//! send a message, answer the permission prompt and read the result.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use coda_client::{Client, Subscription};
use coda_config::config::Dirs;
use coda_config::timing::Timing;
use coda_protocol::methods::{
    DAEMON_STATUS, DaemonStatusParams, DaemonStatusResult, PERMISSION_RESPOND,
    PermissionRespondParams, PermissionRespondResult, SESSION_CREATE, SESSION_RESUME, SESSION_SEND,
    SessionCreateParams, SessionCreateResult, SessionResumeParams, SessionResumeResult,
    SessionSendParams, SessionSendResult,
};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision, RequestId};
use common::{WAIT, short_dir};
use serde_json::Value;

const POLL: Duration = Duration::from_millis(50);
const PERMISSION_NOTIFICATION: &str = "permission.request";

/// Kills the daemon a test started and the backend groups it left in `run/`,
/// even when an assertion fails first.
struct Daemon {
    child: Child,
    run_dir: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let Ok(entries) = std::fs::read_dir(&self.run_dir) else {
            return;
        };
        for path in entries.filter_map(|entry| Some(entry.ok()?.path())) {
            if path.extension().is_none_or(|ext| ext != "pid") {
                continue;
            }
            let pid = std::fs::read(&path)
                .ok()
                .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
                .and_then(|record| record["pid"].as_u64());
            if let Some(pid) = pid {
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{pid}")])
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

fn start_daemon(home: &Path) -> Daemon {
    let fake = coda_testkit::paths::fake_claude_path().unwrap();
    let config = format!(
        "[providers.claude_code]\nenabled = true\nbinary = {:?}\n",
        fake.display().to_string()
    );
    std::fs::write(home.join("config.toml"), config).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_coda"))
        .arg("daemon")
        .env("CODA_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("coda binary should start");
    Daemon {
        child,
        run_dir: Dirs::from_base(home).run_dir(),
    }
}

fn timing() -> Timing {
    Timing {
        idle_timeout: WAIT,
        ..Timing::for_tests()
    }
}

async fn connect(home: &Path) -> Client {
    let socket = Dirs::from_base(home).socket_path();
    tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(client) = Client::connect(&socket, timing()).await {
                return client;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the daemon did not accept connections in time")
}

/// Reads notifications until `matches` accepts one and returns its params.
async fn next_matching(
    subscription: &mut Subscription,
    matches: impl Fn(&str, &Value) -> bool,
) -> Value {
    tokio::time::timeout(WAIT, async {
        while let Some(notification) = subscription.next().await {
            let params = notification.params.unwrap_or(Value::Null);
            if matches(&notification.method, &params) {
                return params;
            }
        }
        panic!("the subscription ended first");
    })
    .await
    .expect("the expected notification did not arrive in time")
}

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
