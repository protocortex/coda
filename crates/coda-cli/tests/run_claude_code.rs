// SPDX-License-Identifier: AGPL-3.0-only

//! `coda run` against the real daemon binary with a delegated Claude Code
//! backend, played by `fake-claude`.

mod common;

use std::process::ExitCode;

use common::{Outcome, StopDaemon, cli, context, short_dir};
use tempfile::TempDir;

struct Rig {
    // First, so the daemon is stopped while its home directory still exists.
    _stop: StopDaemon,
    home: TempDir,
    work: TempDir,
}

fn rig(timing: &str) -> Rig {
    let home = short_dir();
    let work = short_dir();
    let fake = coda_testkit::paths::fake_claude_path().unwrap();
    let config = format!(
        "[providers.claude_code]\nenabled = true\nbinary = {:?}\n\n{timing}",
        fake.display().to_string()
    );
    std::fs::write(home.path().join("config.toml"), config).unwrap();
    Rig {
        _stop: StopDaemon::new(home.path()),
        home,
        work,
    }
}

async fn run(rig: &Rig, prompt: &str) -> Outcome {
    let ctx = context(rig.home.path());
    let cwd = rig.work.path().display().to_string();
    cli(
        &ctx,
        &["coda", "run", "--prompt", prompt, "--cwd", &cwd],
        "",
    )
    .await
}

#[tokio::test]
async fn a_delegated_turn_that_completes_exits_zero() {
    // Arrange
    let rig = rig("");

    // Act
    let outcome = run(&rig, "hello").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
}

#[tokio::test]
async fn a_silent_delegated_backend_ends_the_run_with_an_error_not_a_hang() {
    // Arrange
    let rig = rig("[timing]\nidle_timeout_ms = 300\n");

    // Act
    let outcome = run(&rig, "please stall").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(5), "{}", outcome.err);
    assert!(
        outcome.err.contains("cut short") && outcome.err.contains("idle_timeout"),
        "stderr was: {}",
        outcome.err
    );
}
