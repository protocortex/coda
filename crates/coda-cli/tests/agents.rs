// SPDX-License-Identifier: AGPL-3.0-only

//! `coda agents lint` and `coda agents trust`, in-process and as the real binary.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

use common::{cli, context, short_dir};

const GOOD_AGENT: &str = "+++\nname = \"scout\"\n\
    description = \"Finds code. Use when exploring. Do not use when editing.\"\n\
    tier = \"fast\"\neffort = \"low\"\ntools = [\"read\", \"grep\"]\n+++\nExplore.\n";

fn write(base: &Path, rel: &str, text: &str) {
    let path = base.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A repository root with the user config directory in the same place.
fn repo() -> tempfile::TempDir {
    let dir = short_dir();
    fs::create_dir(dir.path().join(".git")).unwrap();
    dir
}

#[tokio::test]
async fn lint_passes_on_good_files() {
    // Arrange
    let dir = repo();
    write(dir.path(), "agents/scout.md", GOOD_AGENT);
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "agents", "lint"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
}

#[tokio::test]
async fn list_names_the_built_in_agents_and_a_user_file_of_the_same_name_replaces_one() {
    // Arrange
    let dir = repo();
    write(
        dir.path(),
        "agents/reviewer.md",
        &GOOD_AGENT.replace("scout", "reviewer"),
    );
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "agents", "list"], "").await;

    // Assert
    let lines: Vec<&str> = outcome.out.lines().collect();
    assert_eq!(
        lines,
        [
            "agent explore (built-in)",
            "agent implementer (built-in)",
            "agent reviewer (user)",
            "agent tester (built-in)",
        ]
    );
}

#[tokio::test]
async fn lint_names_the_file_and_field_of_every_error_and_fails() {
    // Arrange
    let dir = repo();
    let bad = GOOD_AGENT
        .replace("\"fast\"", "\"turbo\"")
        .replace("\"low\"", "\"extreme\"")
        .replace("\"grep\"", "\"teleport\"");
    write(dir.path(), "agents/scout.md", &bad);
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "agents", "lint"], "").await;

    // Assert
    assert_ne!(outcome.code, ExitCode::SUCCESS);
    for field in ["tier", "effort", "tools"] {
        assert!(
            outcome.err.contains(&format!("scout.md: {field}:")),
            "{}",
            outcome.err
        );
    }
}

#[tokio::test]
async fn lint_rejects_a_model_id_and_a_description_without_guidance() {
    // Arrange
    let dir = repo();
    let bad = GOOD_AGENT
        .replace(
            "Finds code. Use when exploring. Do not use when editing.",
            "Finds code.",
        )
        .replace("tier =", "model = \"claude-opus-4\"\ntier =");
    write(dir.path(), "agents/scout.md", &bad);
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "agents", "lint"], "").await;

    // Assert
    assert_ne!(outcome.code, ExitCode::SUCCESS);
    assert!(outcome.err.contains("scout.md: model:"), "{}", outcome.err);
    assert!(
        outcome.err.contains("scout.md: description:"),
        "{}",
        outcome.err
    );
}

#[tokio::test]
async fn lint_output_carries_no_terminal_escape_from_a_definition_file() {
    // Arrange
    let dir = repo();
    let escaped = GOOD_AGENT.replace("\"grep\"", "\"\\u001b[2Jteleport\"");
    write(dir.path(), "agents/scout.md", &escaped);
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "agents", "lint"], "").await;

    // Assert
    assert_ne!(outcome.code, ExitCode::SUCCESS);
    assert!(outcome.err.contains("teleport"), "{}", outcome.err);
    assert!(!outcome.err.contains('\u{1b}'), "{:?}", outcome.err);
}

#[tokio::test]
async fn trust_makes_project_files_usable_and_revoke_makes_them_pending_again() {
    // Arrange
    let dir = repo();
    write(dir.path(), ".coda/agents/scout.md", GOOD_AGENT);
    let ctx = context(dir.path());

    // Act
    let before = cli(&ctx, &["coda", "agents", "list"], "").await;
    let trust = cli(&ctx, &["coda", "agents", "trust"], "").await;
    let trusted = cli(&ctx, &["coda", "agents", "list"], "").await;
    let revoke = cli(&ctx, &["coda", "agents", "trust", "--revoke"], "").await;
    let after = cli(&ctx, &["coda", "agents", "list"], "").await;

    // Assert
    assert!(before.out.contains("pending"), "{}", before.out);
    assert_eq!(trust.code, ExitCode::SUCCESS, "{}", trust.err);
    assert!(
        trusted.out.contains("agent scout (project)"),
        "{}",
        trusted.out
    );
    assert_eq!(revoke.code, ExitCode::SUCCESS, "{}", revoke.err);
    assert!(after.out.contains("pending"), "{}", after.out);
    assert!(!after.out.contains("agent scout"), "{}", after.out);
}

#[tokio::test]
async fn editing_or_adding_a_project_file_withdraws_trust_until_trusted_again() {
    // Arrange
    let dir = repo();
    write(dir.path(), ".coda/agents/scout.md", GOOD_AGENT);
    let ctx = context(dir.path());
    cli(&ctx, &["coda", "agents", "trust"], "").await;
    let trusted = cli(&ctx, &["coda", "agents", "list"], "").await;

    // Act
    write(
        dir.path(),
        ".coda/agents/scout.md",
        &GOOD_AGENT.replace("Explore.", "Do something else."),
    );
    let edited = cli(&ctx, &["coda", "agents", "list"], "").await;
    let linted = cli(&ctx, &["coda", "agents", "lint"], "").await;
    cli(&ctx, &["coda", "agents", "trust"], "").await;
    write(
        dir.path(),
        ".coda/agents/extra.md",
        &GOOD_AGENT.replace("scout", "extra"),
    );
    let added = cli(&ctx, &["coda", "agents", "list"], "").await;
    cli(&ctx, &["coda", "agents", "trust"], "").await;
    let retrusted = cli(&ctx, &["coda", "agents", "list"], "").await;

    // Assert
    assert!(
        trusted.out.contains("agent scout (project)"),
        "{}",
        trusted.out
    );
    for stale in [&edited, &added] {
        assert!(!stale.out.contains("(project)"), "{}", stale.out);
        assert!(
            stale.out.contains("changed since they were trusted"),
            "{}",
            stale.out
        );
    }
    assert_eq!(linted.code, ExitCode::SUCCESS);
    assert!(
        linted.err.contains("changed since they were trusted"),
        "{}",
        linted.err
    );
    assert!(
        retrusted.out.contains("agent extra (project)"),
        "{}",
        retrusted.out
    );
    assert!(
        retrusted.out.contains("agent scout (project)"),
        "{}",
        retrusted.out
    );
}

#[tokio::test]
async fn trust_outside_a_git_repository_fails() {
    // Arrange
    let dir = short_dir();
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "agents", "trust"], "").await;

    // Assert
    assert_ne!(outcome.code, ExitCode::SUCCESS);
    assert!(outcome.err.contains("git repository"), "{}", outcome.err);
}

#[test]
fn the_binary_lint_exits_non_zero_on_a_bad_file_and_zero_on_a_good_one() {
    // Arrange
    let dir = repo();
    write(dir.path(), "agents/scout.md", GOOD_AGENT);
    let run = |dir: &Path| {
        Command::new(env!("CARGO_BIN_EXE_coda"))
            .args(["agents", "lint"])
            .env("CODA_HOME", dir)
            .current_dir(dir)
            .output()
            .unwrap()
    };

    // Act
    let good = run(dir.path());
    write(
        dir.path(),
        "agents/scout.md",
        &GOOD_AGENT.replace("\"fast\"", "\"turbo\""),
    );
    let bad = run(dir.path());

    // Assert
    assert!(good.status.success());
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("tier"));
}
