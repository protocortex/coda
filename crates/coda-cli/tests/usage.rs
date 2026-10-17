// SPDX-License-Identifier: AGPL-3.0-only

//! `coda usage` against a daemon and, when none answers, against the store.

mod common;

use std::path::Path;
use std::process::ExitCode;

use coda_config::config::Dirs;
use coda_protocol::types::{BackendKind, Mode, SessionId};
use coda_store::{NewSession, SavingRecord, Store, UsageRecord};
use common::{TestDaemon, cli, context, short_dir};

const SUMMED_OUTPUT: &str = "input tokens: 107\n\
output tokens: 23\n\
cache read tokens: 0\n\
cache write tokens: 0\n\
saved bytes: 1100\n";

/// Stores one session with two usage records (100 + 7 in, 20 + 3 out) and two savings, then
/// releases the database so a daemon can open it.
fn seed_usage(base: &Path) -> SessionId {
    let mut store = Store::open(&Dirs::from_base(base).data_dir()).unwrap();
    let session = store
        .create_session(&NewSession {
            backend: BackendKind::Faux,
            model: None,
            cwd: "/r".to_owned(),
            mode: Mode::Default,
        })
        .unwrap();
    let turn = store.begin_turn(&session).unwrap();
    for (input_tokens, output_tokens) in [(100, 20), (7, 3)] {
        store
            .record_usage(&UsageRecord {
                session: session.clone(),
                turn: turn.clone(),
                input_tokens,
                output_tokens,
            })
            .unwrap();
    }
    for (original_bytes, compressed_bytes) in [(1000, 300), (500, 100)] {
        store
            .record_saving(&SavingRecord {
                session: session.clone(),
                original_bytes,
                compressed_bytes,
            })
            .unwrap();
    }
    session
}

#[tokio::test]
async fn usage_prints_tokens_and_savings_from_the_daemon() {
    // Arrange
    let dir = short_dir();
    seed_usage(dir.path());
    let daemon = TestDaemon::start(dir.path());
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "usage"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(outcome.out, SUMMED_OUTPUT);
    daemon.stop().await;
}

#[tokio::test]
async fn usage_for_one_session_asks_the_daemon_for_that_session() {
    // Arrange
    let dir = short_dir();
    let session = seed_usage(dir.path());
    let daemon = TestDaemon::start(dir.path());
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "usage", "--session", session.as_str()], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(outcome.out, SUMMED_OUTPUT);
    daemon.stop().await;
}

#[tokio::test]
async fn usage_reads_the_store_in_process_when_no_daemon_answers() {
    // Arrange
    let dir = short_dir();
    seed_usage(dir.path());
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "usage"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(outcome.out, SUMMED_OUTPUT);
}

#[tokio::test]
async fn usage_does_not_start_a_daemon() {
    // Arrange
    let dir = short_dir();
    let mut ctx = context(dir.path());
    ctx.exe = dir.path().join("no-such-binary");

    // Act
    let outcome = cli(&ctx, &["coda", "usage"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert!(
        outcome.out.starts_with("input tokens: 0\n"),
        "{}",
        outcome.out
    );
}

#[tokio::test]
async fn usage_for_an_unknown_session_fails_instead_of_falling_back() {
    // Arrange
    let dir = short_dir();
    seed_usage(dir.path());
    let daemon = TestDaemon::start(dir.path());
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "usage", "--session", "999"], "").await;

    // Assert
    assert_ne!(outcome.code, ExitCode::SUCCESS);
    assert!(outcome.err.contains("Unknown session"), "{}", outcome.err);
    daemon.stop().await;
}

#[tokio::test]
async fn usage_without_a_database_prints_zeros_and_creates_nothing() {
    // Arrange
    let dir = short_dir();
    let base = dir.path().join("fresh");
    let ctx = context(&base);

    // Act
    let outcome = cli(&ctx, &["coda", "usage"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert!(
        outcome.out.starts_with("input tokens: 0\n"),
        "{}",
        outcome.out
    );
    assert!(!base.exists(), "usage created {}", base.display());
}

#[tokio::test]
async fn usage_against_an_older_daemon_says_to_restart_it() {
    // Arrange
    let dir = short_dir();
    let daemon = TestDaemon::start_without_store(dir.path());
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "usage"], "").await;

    // Assert
    assert_ne!(outcome.code, ExitCode::SUCCESS);
    assert!(
        outcome.err.contains("coda daemon restart"),
        "{}",
        outcome.err
    );
    daemon.stop().await;
}
