// SPDX-License-Identifier: Apache-2.0

//! `coda session list|send|show` against the real daemon binary and a faux provider.

mod common;

use std::process::ExitCode;

use coda_protocol::methods::{SESSION_CREATE, SessionCreateParams, SessionCreateResult};
use coda_protocol::types::{BackendKind, Mode};
use common::{FauxRig as Rig, Outcome, cli, connect, faux_rig, short_dir};
use serde_json::{Value, json};

const EXIT_CONTEXT_OVERFLOW: u8 = 3;
const EXIT_FAILURE: u8 = 70;

fn text_turn(text: &str) -> Value {
    json!({"events": [
        {"type": "text_delta", "text": text},
        {"type": "stop", "reason": "end_turn"},
    ]})
}

fn overflow_turn() -> Value {
    json!({"events": [], "error": {"class": "context_window_exceeded"}})
}

impl Rig {
    async fn coda(&self, args: &[&str]) -> Outcome {
        let mut full = vec!["coda"];
        full.extend_from_slice(args);
        cli(&self.ctx, &full, "").await
    }

    /// Starts the daemon and creates a session that has not run a turn, so
    /// the next send plays the first turn of the script.
    async fn start_session(&self) -> String {
        let started = self.coda(&[]).await;
        assert_eq!(started.code, ExitCode::SUCCESS, "{}", started.err);
        let client = connect(self.home.path()).await;
        let params = SessionCreateParams {
            backend: BackendKind::Faux,
            cwd: self.work.path().display().to_string(),
            model: None,
            mode: Mode::Default,
        };
        let created: SessionCreateResult = client.request(SESSION_CREATE, &params).await.unwrap();
        created.session.id.to_string()
    }
}

#[tokio::test]
async fn list_prints_each_session_as_a_line_and_as_json() {
    // Arrange
    let rig = faux_rig(&[text_turn("one")]);
    let id = rig.start_session().await;

    // Act
    let plain = rig.coda(&["session", "list"]).await;
    let json_out = rig.coda(&["session", "list", "--json"]).await;

    // Assert
    assert_eq!(plain.code, ExitCode::SUCCESS, "{}", plain.err);
    assert!(plain.out.contains(&id), "{}", plain.out);
    assert!(plain.out.contains("idle"), "{}", plain.out);
    let sessions: Vec<Value> = serde_json::from_str(&json_out.out).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["status"], "idle");
}

#[tokio::test]
async fn send_with_wait_prints_the_answer_and_show_replays_the_conversation() {
    // Arrange
    let rig = faux_rig(&[text_turn("two")]);
    let id = rig.start_session().await;

    // Act
    let sent = rig
        .coda(&["session", "send", &id, "--prompt", "second", "--wait"])
        .await;
    let shown = rig.coda(&["session", "show", &id]).await;
    let shown_json = rig.coda(&["session", "show", &id, "--json"]).await;

    // Assert
    assert_eq!(sent.code, ExitCode::SUCCESS, "{}", sent.err);
    assert_eq!(sent.out, "two\n");
    assert!(shown.out.contains("> second"), "{}", shown.out);
    assert!(shown.out.contains("two"), "{}", shown.out);
    let shown: Value = serde_json::from_str(&shown_json.out).unwrap();
    assert_eq!(shown["session"]["id"], id);
    let kinds: Vec<&str> = shown["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.iter().filter(|k| **k == "user_message").count(), 1);
}

#[tokio::test]
async fn send_with_wait_exits_like_run_and_json_carries_the_code() {
    // Arrange
    let rig = faux_rig(&[overflow_turn()]);
    let id = rig.start_session().await;

    // Act
    let sent = rig
        .coda(&[
            "session", "send", &id, "--prompt", "big", "--wait", "--json",
        ])
        .await;

    // Assert
    assert_eq!(
        sent.code,
        ExitCode::from(EXIT_CONTEXT_OVERFLOW),
        "{}",
        sent.err
    );
    let report: Value = serde_json::from_str(&sent.out).unwrap();
    assert_eq!(report["session_id"], id);
    assert_eq!(report["stop"], "context_overflow");
    assert_eq!(report["exit_code"], EXIT_CONTEXT_OVERFLOW);
}

#[tokio::test]
async fn send_without_wait_returns_the_turn_at_once() {
    // Arrange
    let rig = faux_rig(&[text_turn("one"), text_turn("two")]);
    let id = rig.start_session().await;

    // Act
    let sent = rig
        .coda(&["session", "send", &id, "--prompt", "second", "--json"])
        .await;

    // Assert
    assert_eq!(sent.code, ExitCode::SUCCESS, "{}", sent.err);
    let report: Value = serde_json::from_str(&sent.out).unwrap();
    assert_eq!(report["session_id"], id);
    assert!(report["turn_id"].is_string(), "{}", sent.out);
}

#[tokio::test]
async fn an_unknown_session_fails_the_command() {
    // Arrange
    let rig = faux_rig(&[text_turn("one")]);
    rig.start_session().await;

    for args in [
        ["session", "send", "nope", "--prompt", "x"].as_slice(),
        ["session", "show", "nope"].as_slice(),
    ] {
        // Act
        let outcome = rig.coda(args).await;

        // Assert
        assert_eq!(outcome.code, ExitCode::from(EXIT_FAILURE), "{args:?}");
        assert!(outcome.err.starts_with("coda: "), "{}", outcome.err);
    }
}

#[tokio::test]
async fn send_without_a_prompt_is_refused() {
    // Arrange
    let rig = faux_rig(&[text_turn("one")]);
    let id = rig.start_session().await;

    // Act
    let outcome = rig.coda(&["session", "send", &id]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(EXIT_FAILURE));
    assert!(outcome.err.contains("no prompt"), "{}", outcome.err);
}

#[tokio::test]
async fn list_filters_by_a_directory_that_no_longer_exists() {
    // Arrange
    let rig = faux_rig(&[text_turn("one")]);
    let gone = short_dir();
    rig.coda(&[]).await;
    let client = connect(rig.home.path()).await;
    let params = SessionCreateParams {
        backend: BackendKind::Faux,
        cwd: gone.path().display().to_string(),
        model: None,
        mode: Mode::Default,
    };
    let created: SessionCreateResult = client.request(SESSION_CREATE, &params).await.unwrap();
    let id = created.session.id.to_string();
    let path = gone.path().canonicalize().unwrap().display().to_string();
    drop(gone);

    // Act
    let listed = rig.coda(&["session", "list", "--cwd", &path]).await;

    // Assert
    assert_eq!(listed.code, ExitCode::SUCCESS, "{}", listed.err);
    assert!(listed.out.contains(&id), "{}", listed.out);
}

fn run_git(rig: &Rig, args: &[&str]) {
    let done = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(rig.work.path())
        .output()
        .unwrap();
    assert!(done.status.success(), "git {args:?}");
}

/// The id of the first turn of `id`, read from the stored events.
async fn first_turn(rig: &Rig, id: &str) -> String {
    let shown = rig.coda(&["session", "show", id, "--json"]).await;
    let shown: Value = serde_json::from_str(&shown.out).unwrap();
    shown["events"][0]["turn"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn rewind_restores_the_files_marks_later_turns_and_show_says_so() {
    // Arrange
    let rig = faux_rig(&[text_turn("one"), text_turn("two")]);
    std::fs::remove_dir(rig.work.path().join(".git")).unwrap();
    run_git(&rig, &["init", "-q"]);
    std::fs::write(rig.work.path().join("a.txt"), "a\n").unwrap();
    run_git(&rig, &["add", "."]);
    run_git(&rig, &["commit", "-q", "-m", "first"]);
    let id = rig.start_session().await;
    rig.coda(&["session", "send", &id, "--prompt", "first-q", "--wait"])
        .await;
    std::fs::write(rig.work.path().join("later.txt"), "later\n").unwrap();
    rig.coda(&["session", "send", &id, "--prompt", "second-q", "--wait"])
        .await;
    let turn = first_turn(&rig, &id).await;

    // Act
    let rewound = rig
        .coda(&["session", "rewind", &id, "--to", &turn, "--json"])
        .await;
    let shown = rig.coda(&["session", "show", &id]).await;
    let shown_json = rig.coda(&["session", "show", &id, "--json"]).await;

    // Assert
    assert_eq!(rewound.code, ExitCode::SUCCESS, "{}", rewound.err);
    let result: Value = serde_json::from_str(&rewound.out).unwrap();
    assert_eq!(result["files_restored"], true);
    assert_eq!(result["turns_rolled_back"], 1);
    let saved = result["saved_as"].as_str().unwrap();
    assert!(saved.starts_with("refs/coda/checkpoints/"), "{saved}");
    assert!(!rig.work.path().join("later.txt").exists());
    assert!(shown.out.contains("> first-q"), "{}", shown.out);
    assert!(
        shown.out.contains("> [rolled back] second-q"),
        "{}",
        shown.out
    );
    let shown: Value = serde_json::from_str(&shown_json.out).unwrap();
    let flags: Vec<bool> = shown["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "user_message")
        .map(|e| e["rolled_back"].as_bool().unwrap())
        .collect();
    assert_eq!(flags, [false, true]);
}

#[tokio::test]
async fn rewind_to_base_rolls_back_every_turn_and_keep_files_leaves_the_tree() {
    // Arrange
    let rig = faux_rig(&[text_turn("one")]);
    let id = rig.start_session().await;
    rig.coda(&["session", "send", &id, "--prompt", "first-q", "--wait"])
        .await;
    std::fs::write(rig.work.path().join("mine.txt"), "mine\n").unwrap();

    // Act
    let rewound = rig
        .coda(&["session", "rewind", &id, "--to", "base", "--keep-files"])
        .await;

    // Assert
    assert_eq!(rewound.code, ExitCode::SUCCESS, "{}", rewound.err);
    assert!(
        rewound.out.contains("rolled back 1 turn"),
        "{}",
        rewound.out
    );
    assert!(rig.work.path().join("mine.txt").exists());
}

#[tokio::test]
async fn a_refused_rewind_fails_the_command_with_the_reason() {
    // Arrange
    let rig = faux_rig(&[text_turn("one")]);
    let id = rig.start_session().await;
    rig.coda(&["session", "send", &id, "--prompt", "first-q", "--wait"])
        .await;

    // Act
    let rewound = rig.coda(&["session", "rewind", &id, "--to", "base"]).await;

    // Assert
    assert_eq!(
        rewound.code,
        ExitCode::from(EXIT_FAILURE),
        "{}",
        rewound.out
    );
    assert!(rewound.err.contains("snapshot"), "{}", rewound.err);
}
