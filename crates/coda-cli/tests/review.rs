// SPDX-License-Identifier: Apache-2.0

//! `coda review` against the real daemon binary, with the reviewer answering from a faux script or a fake Claude Code.

mod common;

use std::path::Path;
use std::process::{Command, ExitCode};

use coda_cli::Context;
use coda_review::schema::{Review, Risk, Severity};
use common::{FauxRig, Outcome, StopDaemon, cli, context, faux_rig, short_dir};
use serde_json::{Value, json};
use tempfile::TempDir;

const BASE_FILE: &str = "one\ntwo\nthree\n";
const TOPIC_FILE: &str = "one\ntwo\nthree\nfour\nfive\n";
const REVIEWER: &str = "reviewer";
const FAILURE: u8 = 70;
const USAGE: u8 = 64;

fn text_turn(text: &str) -> Value {
    json!({"events": [
        {"type": "text_delta", "text": text},
        {"type": "usage", "input_tokens": 11, "output_tokens": 7},
        {"type": "stop", "reason": "end_turn"},
    ]})
}

fn tool_turn(name: &str, arguments: &Value) -> Value {
    json!({"events": [
        {"type": "tool_call_start", "call_id": "c1", "name": name},
        {"type": "tool_call_arguments_delta", "call_id": "c1", "fragment": arguments.to_string()},
        {"type": "usage", "input_tokens": 5, "output_tokens": 3},
        {"type": "stop", "reason": "tool_use"},
    ]})
}

fn task_turn() -> Value {
    tool_turn(
        "task",
        &json!({"agent": REVIEWER, "prompt": "Review the file."}),
    )
}

fn review_json(line: u32) -> String {
    json!({
        "summary": "Adds two lines.",
        "effort": 1,
        "risk": "medium",
        "findings": [{
            "file": "a.txt", "start_line": line, "end_line": line, "severity": "high",
            "category": "bug", "title": "Wrong word", "explanation": "Four is not a number here.",
            "suggested_patch": null, "confidence": "medium"
        }]
    })
    .to_string()
}

/// The turns of one successful review: the session hands off, the reviewer answers, the session ends.
fn answered(reply: &str) -> Vec<Value> {
    vec![task_turn(), text_turn(reply), text_turn("done")]
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "{args:?}");
}

/// A faux rig whose work directory is a repository with `main` and a `topic` branch
/// that adds two lines to `a.txt`, checked out.
fn rig(script: &[Value]) -> FauxRig {
    let rig = faux_rig(script);
    let dir = rig.work.path();
    std::fs::remove_dir_all(dir.join(".git")).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), BASE_FILE).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "base"]);
    git(dir, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(dir.join("a.txt"), TOPIC_FILE).unwrap();
    git(dir, &["commit", "-q", "-am", "topic"]);
    rig
}

async fn review(rig: &FauxRig, flags: &[&str]) -> Outcome {
    let cwd = rig.work.path().display().to_string();
    let mut args = vec!["coda", "review", "--cwd", &cwd];
    args.extend_from_slice(flags);
    cli(&rig.ctx, &args, "").await
}

#[tokio::test]
async fn a_review_prints_the_findings_as_markdown_and_exits_zero() {
    // Arrange
    let rig = rig(&answered(&review_json(4)));

    // Act
    let outcome = review(&rig, &[]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert!(outcome.out.starts_with("# Review\n"), "{}", outcome.out);
    assert!(outcome.out.contains("Wrong word"), "{}", outcome.out);
    assert!(outcome.out.contains("`a.txt:4-4`"), "{}", outcome.out);
}

#[tokio::test]
async fn json_output_is_a_review_that_fits_the_schema() {
    // Arrange
    let rig = rig(&answered(&review_json(5)));

    // Act
    let outcome = review(&rig, &["--json"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let parsed: Review = serde_json::from_str(&outcome.out).unwrap();
    assert_eq!(parsed.risk, Risk::Medium);
    assert_eq!(parsed.findings.len(), 1);
    assert_eq!(parsed.findings[0].id, "F1");
    assert_eq!(parsed.findings[0].severity, Severity::High);
    assert_eq!(parsed.findings[0].start_line, 5);
    assert_eq!(parsed.coverage.reviewed, ["a.txt"]);
    assert_eq!(parsed.run.dropped_findings, 0);
}

#[tokio::test]
async fn the_reviewer_has_read_only_tools_and_can_read_the_diff_it_was_given() {
    // Arrange
    let path = format!(".coda/review/input-{}.txt", std::process::id());
    let script = vec![
        task_turn(),
        tool_turn("read", &json!({"path": path})),
        text_turn(&review_json(4)),
        text_turn("done"),
    ];
    let rig = rig(&script);

    // Act
    let outcome = review(&rig, &["--json", "--instructions", "watch the numbers"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let requests = rig.requests();
    let child_tools = requests[1]["tools"].as_array().unwrap();
    for tool in child_tools {
        let name = tool.as_str().unwrap();
        assert!(!["bash", "edit", "task"].contains(&name), "{name}");
    }
    let after_read = requests[2]["messages"].to_string();
    assert!(after_read.contains("+four"), "{after_read}");
    assert!(after_read.contains("watch the numbers"), "{after_read}");
    assert!(!rig.work.path().join(".coda/review").exists());
}

#[tokio::test]
async fn two_bad_shaped_answers_fail_with_a_clear_error() {
    // Arrange
    let mut script = answered("not json");
    script.extend(answered("still not json"));
    let rig = rig(&script);

    // Act
    let outcome = review(&rig, &[]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(FAILURE), "{}", outcome.out);
    assert!(outcome.err.contains("bad shape"), "{}", outcome.err);
    assert_eq!(outcome.out, "");
}

#[tokio::test]
async fn a_backend_failure_is_a_clear_error_and_a_non_zero_exit() {
    // Arrange
    let down = json!({"events": [], "error": {"class": "fatal", "message": "the model is down"}});
    let rig = rig(&[down]);

    // Act
    let outcome = review(&rig, &[]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(FAILURE), "{}", outcome.out);
    assert!(
        outcome.err.starts_with("coda: the review failed"),
        "{}",
        outcome.err
    );
    assert_eq!(outcome.out, "");
}

#[tokio::test]
async fn a_reviewer_that_fails_leaves_no_review_and_a_non_zero_exit() {
    // Arrange
    let down = json!({"events": [], "error": {"class": "fatal", "message": "the model is down"}});
    let rig = rig(&[task_turn(), down, text_turn("done")]);

    // Act
    let outcome = review(&rig, &[]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(FAILURE), "{}", outcome.out);
    assert!(outcome.err.contains("reviewer"), "{}", outcome.err);
}

#[tokio::test]
async fn staged_changes_are_reviewed_instead_of_the_branch() {
    // Arrange
    let rig = rig(&answered(&review_json(2)));
    let dir = rig.work.path();
    std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\nfour\nfive\n").unwrap();
    git(dir, &["add", "a.txt"]);

    // Act
    let outcome = review(&rig, &["--staged", "--json"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let parsed: Review = serde_json::from_str(&outcome.out).unwrap();
    assert_eq!(parsed.findings[0].start_line, 2);
}

#[tokio::test]
async fn a_change_with_nothing_to_review_makes_no_model_call() {
    // Arrange
    let rig = rig(&[]);

    // Act
    let outcome = review(&rig, &["main...main"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert!(outcome.out.contains("nothing to review"), "{}", outcome.out);
    assert_eq!(rig.requests(), Vec::<Value>::new());
}

#[tokio::test]
async fn a_branch_name_is_reviewed_against_the_default_branch() {
    // Arrange
    let rig = rig(&answered(&review_json(4)));
    git(rig.work.path(), &["checkout", "-q", "main"]);

    // Act
    let outcome = review(&rig, &["topic", "--json"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let parsed: Review = serde_json::from_str(&outcome.out).unwrap();
    assert_eq!(parsed.findings.len(), 1);
}

const PROMPTS: &str = "prompts.txt";

/// A home that enables only Claude Code, played by `fake-claude`, and a work directory
/// that is the repository of [`rig`]. The fake answers with `reply` after some narration.
struct ClaudeRig {
    // First, so the daemon is stopped while its home directory still exists.
    _stop: StopDaemon,
    _home: TempDir,
    work: TempDir,
    ctx: Context,
}

fn claude_rig(reply: &str) -> ClaudeRig {
    let home = short_dir();
    let work = short_dir();
    let fake = coda_testkit::paths::fake_claude_path().unwrap();
    let config = format!(
        "[providers.claude_code]\nenabled = true\nbinary = {:?}\n",
        fake.display().to_string()
    );
    std::fs::write(home.path().join("config.toml"), config).unwrap();
    let dir = work.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), BASE_FILE).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "base"]);
    git(dir, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(dir.join("a.txt"), TOPIC_FILE).unwrap();
    git(dir, &["commit", "-q", "-am", "topic"]);
    let control = json!({"echo_text": reply, "prompt_path": dir.join(PROMPTS)});
    std::fs::write(dir.join("fake-claude-control.json"), control.to_string()).unwrap();
    ClaudeRig {
        ctx: context(home.path()),
        _stop: StopDaemon::new(home.path()),
        _home: home,
        work,
    }
}

impl ClaudeRig {
    async fn review(&self, flags: &[&str]) -> Outcome {
        let cwd = self.work.path().display().to_string();
        let mut args = vec![
            "coda",
            "review",
            "--cwd",
            &cwd,
            "--instructions",
            "echo-text",
        ];
        args.extend_from_slice(flags);
        cli(&self.ctx, &args, "").await
    }

    fn prompts(&self) -> String {
        std::fs::read_to_string(self.work.path().join(PROMPTS)).unwrap_or_default()
    }
}

#[tokio::test]
async fn a_claude_code_review_gets_the_diff_in_the_prompt_and_leaves_no_hand_off_file() {
    // Arrange
    let rig = claude_rig(&review_json(4));

    // Act
    let outcome = rig.review(&["--backend", "claude-code", "--json"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let parsed: Review = serde_json::from_str(&outcome.out).unwrap();
    assert_eq!(parsed.findings.len(), 1);
    assert_eq!(parsed.findings[0].start_line, 4);
    assert_eq!(parsed.run.model, "claude_code");
    let prompts = rig.prompts();
    assert!(prompts.contains("You are a code reviewer"), "{prompts}");
    assert!(prompts.contains("+four"), "{prompts}");
    assert!(!prompts.contains(".coda/review"), "{prompts}");
    assert!(!rig.work.path().join(".coda").exists());
}

#[tokio::test]
async fn claude_code_is_the_default_when_it_is_the_only_backend_enabled() {
    // Arrange
    let rig = claude_rig(&review_json(5));

    // Act
    let outcome = rig.review(&["--json"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let parsed: Review = serde_json::from_str(&outcome.out).unwrap();
    assert_eq!(parsed.findings[0].start_line, 5);
}

#[tokio::test]
async fn a_claude_code_answer_of_the_wrong_shape_is_asked_for_once_more_with_a_hint() {
    // Arrange
    let rig = claude_rig("not json");

    // Act
    let outcome = rig.review(&[]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(FAILURE), "{}", outcome.out);
    assert!(outcome.err.contains("bad shape"), "{}", outcome.err);
    let prompts = rig.prompts();
    assert_eq!(
        prompts.matches("You are a code reviewer").count(),
        2,
        "{prompts}"
    );
    assert!(
        prompts.contains("Your last answer was rejected"),
        "{prompts}"
    );
}

#[tokio::test]
async fn staged_cannot_be_combined_with_a_target() {
    // Arrange
    let rig = rig(&[]);

    // Act
    let outcome = review(&rig, &["--staged", "topic"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(USAGE), "{}", outcome.err);
}

#[tokio::test]
async fn json_output_stays_a_review_when_there_is_nothing_to_review() {
    // Arrange
    let rig = rig(&[]);

    // Act
    let outcome = review(&rig, &["main...main", "--json"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let parsed: Review = serde_json::from_str(&outcome.out).unwrap();
    assert!(parsed.summary.contains("nothing to review"));
    assert_eq!(parsed.findings.len(), 0);
}
