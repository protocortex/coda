// SPDX-License-Identifier: AGPL-3.0-only

//! A crashed backend is restarted by the supervisor, against the scripted `fake-claude`.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use coda_protocol::methods::SessionSendParams;
use rustix::process::{Pid, test_kill_process};
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::{POLL, RESTART_BASE, Rig, TurnConfig, Turns, WAIT, relaxed};
use crate::supervisor::{RestartPolicy, Sleeper, Supervisor};

/// A sleeper that parks every backoff until the test releases it.
#[derive(Debug, Default)]
struct GatedSleeper {
    entered: Notify,
    release: Notify,
}

impl GatedSleeper {
    async fn wait_entered(&self) {
        tokio::time::timeout(WAIT, self.entered.notified())
            .await
            .expect("the supervisor never began a backoff");
    }
}

impl Sleeper for GatedSleeper {
    fn sleep(&self, _: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {
            self.entered.notify_one();
            self.release.notified().await;
        })
    }
}

impl Rig {
    async fn count(&self, kind: &str) -> usize {
        self.kinds().await.iter().filter(|k| *k == kind).count()
    }

    /// The `will_restart` flag of each stored crash event, in order.
    async fn crash_restart_flags(&self) -> Vec<bool> {
        self.events()
            .await
            .iter()
            .filter(|e| e.kind == "backend_crashed")
            .map(|e| {
                let body: Value = serde_json::from_str(&e.body).unwrap();
                body["will_restart"].as_bool().unwrap_or(false)
            })
            .collect()
    }

    /// Turns over the rig's store that run `binary` and wait at `sleeper` before every restart.
    fn turns_with(&self, binary: PathBuf, sleeper: Arc<dyn Sleeper>) -> Arc<Turns> {
        let supervisor = Supervisor::new(RestartPolicy::new(RESTART_BASE), sleeper);
        let config = TurnConfig {
            binary: Some(binary),
            pidfile_dir: self.data.path().to_path_buf(),
            parent_env: Vec::new(),
            timing: relaxed(),
            models: coda_config::config::Config::default(),
        };
        Turns::with_supervisor(Arc::clone(&self.hub), config, supervisor)
    }

    fn gated_turns(&self, gate: &Arc<GatedSleeper>) -> Arc<Turns> {
        let binary = coda_testkit::paths::fake_claude_path().unwrap();
        self.turns_with(binary, Arc::clone(gate) as Arc<_>)
    }

    async fn send_to(&self, turns: &Arc<Turns>, text: &str) {
        let params = SessionSendParams {
            session_id: self.session.clone(),
            text: text.to_owned(),
            max_turns: None,
            token_cap: None,
        };
        turns.send(params).await.unwrap();
    }

    async fn wait_until_stopped(&self, turns: &Turns) {
        let path = self.pidfile();
        tokio::time::timeout(WAIT, async {
            while path.exists() || turns.is_running(&self.session) {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("the turn did not clean up in time");
    }
}

fn marker_control(rig: &Rig, extra: &Value) {
    let marker = rig.cwd.path().join("crashed-once");
    let mut control = json!({ "crash_marker": marker });
    control
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    rig.control(&control);
}

fn pids(log: &Path) -> Vec<i32> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.parse().ok())
        .collect()
}

fn backend_pid(rig: &Rig) -> i32 {
    let record: Value = serde_json::from_slice(&std::fs::read(rig.pidfile()).unwrap()).unwrap();
    i32::try_from(record["pid"].as_u64().unwrap()).unwrap()
}

fn alive(pid: i32) -> bool {
    Pid::from_raw(pid).is_some_and(|pid| test_kill_process(pid).is_ok())
}

async fn eventually(what: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} did not happen in time"));
}

#[tokio::test]
async fn a_crashed_backend_is_restarted_after_the_backoff_and_the_turn_completes() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    marker_control(&rig, &json!({}));

    // Act
    rig.send("flaky").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(rig.sleeper.waits(), [RESTART_BASE]);
    assert_eq!(rig.count("backend_crashed").await, 1);
}

#[tokio::test]
async fn the_crash_event_names_the_session_and_the_exit_code_only() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    marker_control(&rig, &json!({}));

    // Act
    rig.send("flaky").await;
    let crash = rig.wait_for("backend_crashed").await;

    // Assert
    let body: Value = serde_json::from_str(&crash.body).unwrap();
    assert_eq!(
        body,
        json!({
            "type": "backend_crashed",
            "session_id": rig.session,
            "exit_code": 3,
            "will_restart": true
        })
    );
}

#[tokio::test]
async fn a_backend_that_keeps_crashing_ends_the_turn_as_failed_after_the_cap() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    rig.send("please crash").await;
    let result = rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let body: Value = serde_json::from_str(&result.body).unwrap();
    assert_eq!(body["is_error"], true);
    assert_eq!(
        rig.crash_restart_flags().await,
        [true, true, true, false],
        "the last crash must say no restart follows"
    );
    assert_eq!(
        rig.sleeper.waits(),
        [RESTART_BASE, RESTART_BASE * 2, RESTART_BASE * 4]
    );
}

#[tokio::test]
async fn a_user_cancel_is_not_restarted() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("hello").await;
    rig.wait_for_pending().await;

    // Act
    rig.call("session.cancel", json!({"session_id": rig.session}))
        .await;
    rig.wait_for("turn_interrupted").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(rig.sleeper.waits(), Vec::<Duration>::new());
    assert_eq!(rig.count("backend_crashed").await, 0);
}

#[tokio::test]
async fn a_clean_exit_is_not_restarted() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;

    // Act
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(rig.sleeper.waits(), Vec::<Duration>::new());
    assert_eq!(rig.count("backend_crashed").await, 0);
}

#[tokio::test]
async fn a_restart_leaves_no_process_of_the_crashed_backend_behind() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let log = rig.cwd.path().join("grandchildren");
    marker_control(&rig, &json!({ "pid_log": log }));

    // Act
    rig.send("grandchild flaky").await;
    eventually("both grandchildren starting", || pids(&log).len() == 2).await;
    let (old, new) = (pids(&log)[0], pids(&log)[1]);
    eventually("the old group ending", || !alive(old)).await;
    let new_alive = alive(new);
    rig.call("session.cancel", json!({"session_id": rig.session}))
        .await;
    rig.wait_until_idle().await;

    // Assert
    assert!(new_alive);
    eventually("the new group ending", || !alive(new)).await;
}

#[tokio::test]
async fn shutting_down_during_the_backoff_stops_the_supervisor() {
    // Arrange
    let rig = Rig::new(relaxed(), Vec::new());
    let gate = Arc::new(GatedSleeper::default());
    let turns = rig.gated_turns(&gate);
    rig.send_to(&turns, "please crash").await;
    gate.wait_entered().await;

    // Act
    turns.shutdown().await;
    gate.release.notify_one();
    rig.wait_until_stopped(&turns).await;

    // Assert
    assert_eq!(rig.count("backend_crashed").await, 1);
    assert_eq!(rig.count("turn_interrupted").await, 1);
}

#[tokio::test]
async fn a_cancel_that_lands_during_the_backoff_wins_over_the_restart() {
    // Arrange
    let rig = Rig::new(relaxed(), Vec::new());
    let gate = Arc::new(GatedSleeper::default());
    let turns = rig.gated_turns(&gate);
    rig.send_to(&turns, "please crash").await;
    gate.wait_entered().await;

    // Act
    let cancelled = turns.cancel(&rig.session);
    gate.release.notify_one();
    rig.wait_until_stopped(&turns).await;

    // Assert
    assert!(cancelled);
    assert_eq!(rig.count("backend_crashed").await, 1);
    assert_eq!(rig.count("turn_interrupted").await, 1);
}

#[tokio::test]
async fn a_cancel_racing_a_crash_ends_the_turn_without_a_restart_or_a_stray_process() {
    // Arrange
    let rig = Rig::new(relaxed(), Vec::new());
    let gate = Arc::new(GatedSleeper::default());
    let turns = rig.gated_turns(&gate);
    rig.send_to(&turns, "please crash").await;
    let backend = backend_pid(&rig);

    // Act
    turns.cancel(&rig.session);
    gate.release.notify_one();
    gate.release.notify_one();
    gate.release.notify_one();
    rig.wait_until_stopped(&turns).await;

    // Assert
    eventually("the backend process ending", || !alive(backend)).await;
    assert!(!rig.pidfile().exists());
    assert!(rig.crash_restart_flags().await.iter().all(|flag| !flag));
    assert!(rig.count("backend_crashed").await <= 1);
    assert_eq!(rig.count("permission_request").await, 0);
}

#[tokio::test]
async fn a_crash_after_a_tool_was_approved_fails_the_turn_without_replaying_the_prompt() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    rig.send("tooldie").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    let result = rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let body: Value = serde_json::from_str(&result.body).unwrap();
    assert_eq!(body["is_error"], true);
    assert_eq!(rig.crash_restart_flags().await, [false]);
    assert_eq!(rig.sleeper.waits(), Vec::<Duration>::new());
    assert_eq!(rig.count("permission_request").await, 1);
    assert_eq!(rig.count("approve").await, 1);
}

#[tokio::test]
async fn a_restart_that_cannot_start_the_backend_fails_the_turn() {
    // Arrange
    let rig = Rig::new(relaxed(), Vec::new());
    let binary = rig.data.path().join("fake-claude-copy");
    std::fs::copy(coda_testkit::paths::fake_claude_path().unwrap(), &binary).unwrap();
    let turns = rig.turns_with(binary, Arc::clone(&rig.sleeper) as Arc<_>);

    // Act
    rig.send_to(&turns, "vanish").await;
    let result = rig.wait_for("result").await;
    rig.wait_until_stopped(&turns).await;

    // Assert
    let body: Value = serde_json::from_str(&result.body).unwrap();
    assert_eq!(body["is_error"], true);
    assert_eq!(rig.crash_restart_flags().await, [true]);
    assert_eq!(rig.sleeper.waits(), [RESTART_BASE]);
}

#[tokio::test]
async fn a_restart_stopped_by_the_daemon_shutting_down_is_not_reported_as_cancelled() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let turns = Arc::clone(&rig.turns);
    // The daemon starts shutting down while the crashed backend waits to be restarted.
    rig.sleeper.on_sleep(move || {
        turns
            .stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
    });

    // Act
    rig.send("please crash").await;
    rig.wait_for("turn_interrupted").await;
    rig.wait_until_idle().await;

    // Assert
    let interrupted = rig
        .events()
        .await
        .into_iter()
        .find(|e| e.kind == "turn_interrupted")
        .unwrap();
    let body: Value = serde_json::from_str(&interrupted.body).unwrap();
    assert_eq!(body["cause"], "daemon_stopping");
}
