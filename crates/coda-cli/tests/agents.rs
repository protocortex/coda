// SPDX-License-Identifier: AGPL-3.0-only

//! `coda agents trust` and `coda agents list`.

mod common;

use std::fs;
use std::path::Path;
use std::process::ExitCode;

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
