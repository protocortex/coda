// SPDX-License-Identifier: Apache-2.0

//! `coda usage --history`: tokens per day, model and project from the transcripts
//! the vendor CLIs keep, read without a daemon or a database.

mod common;

use std::fs;
use std::process::ExitCode;

use coda_providers::transcripts::TranscriptRoots;
use common::{cli, context, short_dir};
use serde_json::json;

const USAGE_EXIT: u8 = 64;

fn reply(id: &str, day: &str, model: &str, output: u64) -> String {
    json!({
        "type": "assistant", "requestId": id, "cwd": "/work/app",
        "timestamp": format!("{day}T10:00:00Z"),
        "message": {"id": id, "model": model, "usage": {
            "input_tokens": 2, "output_tokens": output,
            "cache_read_input_tokens": 100, "cache_creation_input_tokens": 50}},
    })
    .to_string()
}

fn rig(lines: &[String]) -> (tempfile::TempDir, TranscriptRoots) {
    let home = short_dir();
    let claude = home.path().join("claude-projects");
    fs::create_dir_all(claude.join("p")).unwrap();
    fs::write(claude.join("p/a.jsonl"), lines.join("\n") + "\n").unwrap();
    let roots = TranscriptRoots {
        claude: Some(claude),
        codex: Some(home.path().join("codex-sessions")),
    };
    (home, roots)
}

fn words(line: &str) -> Vec<&str> {
    line.split_whitespace().collect()
}

#[tokio::test]
async fn history_lists_each_day_model_and_project_with_totals_per_tool() {
    // Arrange
    let (home, roots) = rig(&[
        reply("m1", "2026-10-27", "claude-haiku", 10),
        reply("m2", "2026-10-27", "claude-haiku", 20),
        reply("m3", "2026-10-28", "claude-opus", 5),
    ]);
    let mut ctx = context(home.path());
    ctx.transcripts = roots;

    // Act
    let outcome = cli(&ctx, &["coda", "usage", "--history"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let lines: Vec<Vec<&str>> = outcome.out.lines().map(words).collect();
    assert!(lines.contains(&vec![
        "2026-10-27",
        "claude",
        "claude-haiku",
        "/work/app",
        "4",
        "30",
        "200",
        "100"
    ]));
    assert!(lines.contains(&vec![
        "2026-10-28",
        "claude",
        "claude-opus",
        "/work/app",
        "2",
        "5",
        "100",
        "50"
    ]));
    assert!(
        lines.contains(&vec!["total", "claude", "6", "35", "300", "150"]),
        "{}",
        outcome.out
    );
}

#[tokio::test]
async fn history_says_when_there_is_nothing_to_read_and_how_many_lines_it_skipped() {
    // Arrange
    let (home, roots) = rig(&["{broken".to_owned()]);
    let mut ctx = context(home.path());
    ctx.transcripts = roots;

    // Act
    let outcome = cli(&ctx, &["coda", "usage", "--history"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS);
    assert!(
        outcome.out.contains("no transcript usage found"),
        "{}",
        outcome.out
    );
    assert!(outcome.out.contains("skipped 1 line"), "{}", outcome.out);
}

#[tokio::test]
async fn history_does_not_combine_with_a_session() {
    // Arrange
    let home = short_dir();

    // Act
    let outcome = cli(
        &context(home.path()),
        &["coda", "usage", "--history", "--session", "s-1"],
        "",
    )
    .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(USAGE_EXIT));
}
