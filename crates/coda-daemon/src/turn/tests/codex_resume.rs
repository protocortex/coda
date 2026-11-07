// SPDX-License-Identifier: Apache-2.0

//! A restarted daemon resumes a Codex thread that Codex kept, and falls back to
//! the text seed when it cannot.

use std::sync::Arc;

use coda_protocol::types::BackendKind;
use serde_json::json;

use super::{POLL, RESTART_BASE, Rig, TurnConfig, Turns, WAIT, codex_config, relaxed};
use crate::supervisor::{RestartPolicy, Supervisor, TokioSleeper};

const THREAD_ID: &str = "fake-thread-1";

fn persisting_config() -> coda_config::config::Config {
    let binary = coda_testkit::paths::fake_codex_path().unwrap();
    let toml = format!(
        "[providers.codex]\nenabled = true\nbinary = \"{}\"\nverified_versions = [\"0.0.0-fake\"]\npersist_threads = true\n",
        binary.display()
    );
    coda_config::config::Config::from_toml_str(&toml)
        .unwrap()
        .config
}

fn rig_with(config: coda_config::config::Config) -> Rig {
    let rig = Rig::build(relaxed(), Vec::new(), BackendKind::CodexAppServer, config);
    rig.write_codex_control(&json!({}));
    rig
}

/// What a new daemon over the same store would run: new `Turns`, same config.
fn restarted(rig: &Rig, config: coda_config::config::Config) -> Arc<Turns> {
    let supervisor = Supervisor::new(RestartPolicy::new(RESTART_BASE), Arc::new(TokioSleeper));
    Turns::with_supervisor(
        Arc::clone(&rig.hub),
        TurnConfig {
            binary: Some(coda_testkit::paths::fake_claude_path().unwrap()),
            pidfile_dir: rig.data.path().to_path_buf(),
            parent_env: Vec::new(),
            timing: relaxed(),
            models: config,
        },
        supervisor,
    )
}

async fn first_turn_then_restart(rig: &mut Rig, config: coda_config::config::Config) -> Arc<Turns> {
    rig.send("hello").await;
    rig.wait_until_turn_over().await;
    rig.turns.shutdown().await;
    restarted(rig, config)
}

async fn second_turn(rig: &Rig, turns: &Arc<Turns>) {
    rig.send_to(turns, "again").await;
    tokio::time::timeout(WAIT, async {
        while turns.is_running(&rig.session) {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the turn did not end in time");
}

async fn stored_thread(rig: &Rig) -> Option<String> {
    let session = rig.session.clone();
    rig.hub
        .with_store(move |store| store.active_backend_ref(&session, BackendKind::CodexAppServer))
        .await
        .unwrap()
}

fn prompts(rig: &Rig) -> String {
    rig.codex_report()["prompts"].to_string()
}

#[tokio::test]
async fn a_kept_thread_is_resumed_after_a_restart_without_the_text_seed() {
    // Arrange
    let config = persisting_config();
    let mut rig = rig_with(config.clone());
    let turns = first_turn_then_restart(&mut rig, config).await;

    // Act
    second_turn(&rig, &turns).await;

    // Assert
    let report = rig.codex_report();
    assert_eq!(report["thread_resume"]["threadId"], THREAD_ID);
    assert!(report["thread_start"].is_null(), "{report}");
    assert_eq!(report["prompts"], json!(["again"]));
}

#[tokio::test]
async fn a_refused_resume_falls_back_to_a_new_thread_with_the_text_seed() {
    // Arrange
    let config = persisting_config();
    let mut rig = rig_with(config.clone());
    let turns = first_turn_then_restart(&mut rig, config).await;
    rig.write_codex_control(&json!({"reject_resume": true}));

    // Act
    second_turn(&rig, &turns).await;

    // Assert
    let report = rig.codex_report();
    assert!(report["thread_start"].is_object(), "{report}");
    let seeded = prompts(&rig);
    assert!(
        seeded.contains("hello") && seeded.contains("again"),
        "{seeded}"
    );
}

#[tokio::test]
async fn without_persist_threads_a_restart_still_starts_a_seeded_ephemeral_thread() {
    // Arrange
    let config = codex_config();
    let mut rig = rig_with(config.clone());
    let turns = first_turn_then_restart(&mut rig, config).await;

    // Act
    second_turn(&rig, &turns).await;

    // Assert
    let report = rig.codex_report();
    assert!(report["thread_resume"].is_null(), "{report}");
    assert_eq!(report["thread_start"]["ephemeral"], true);
    assert!(prompts(&rig).contains("hello"));
}

#[tokio::test]
async fn a_rewind_forgets_the_kept_thread_so_it_is_not_resumed() {
    // Arrange
    let config = persisting_config();
    let mut rig = rig_with(config.clone());
    rig.send("hello").await;
    rig.wait_until_turn_over().await;
    let before = stored_thread(&rig).await;

    // Act
    rig.call(
        "session.rewind",
        json!({"session_id": rig.session, "keep_files": true}),
    )
    .await;

    // Assert
    assert_eq!(before.as_deref(), Some(THREAD_ID));
    assert_eq!(stored_thread(&rig).await, None::<String>);
}

#[tokio::test]
async fn a_thread_kept_before_persist_threads_was_turned_off_is_not_resumed_later() {
    // Arrange
    let on = persisting_config();
    let mut rig = rig_with(on.clone());
    let off = codex_config();
    let turns = first_turn_then_restart(&mut rig, off.clone()).await;
    second_turn(&rig, &turns).await;
    turns.shutdown().await;
    let again = restarted(&rig, on);

    // Act
    second_turn(&rig, &again).await;

    // Assert
    let report = rig.codex_report();
    assert!(report["thread_resume"].is_null(), "{report}");
    assert_eq!(report["thread_start"]["ephemeral"], false);
}

#[tokio::test]
async fn a_stored_thread_stays_until_a_turn_has_run_on_its_replacement() {
    // Arrange
    let config = persisting_config();
    let rig = rig_with(config.clone());
    let session = rig.session.clone();
    rig.hub
        .with_store_mut(move |store| {
            store.record_backend_thread(&session, BackendKind::CodexAppServer, "saved-elsewhere")
        })
        .await
        .unwrap();
    rig.write_codex_control(&json!({"reject_resume": true}));
    let turns = restarted(&rig, config);

    // Act
    rig.send_to(&turns, "please stall").await;
    rig.wait_for_prompts(1).await;
    let during = stored_thread(&rig).await;
    turns.cancel(&rig.session);
    tokio::time::timeout(WAIT, async {
        while turns.is_running(&rig.session) {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the turn did not end in time");

    // Assert
    assert_eq!(during.as_deref(), Some("saved-elsewhere"));
    assert_eq!(stored_thread(&rig).await.as_deref(), Some(THREAD_ID));
}
