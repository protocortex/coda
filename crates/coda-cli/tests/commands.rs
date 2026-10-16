// SPDX-License-Identifier: AGPL-3.0-only

//! Each command run in-process against a daemon on a short temporary socket.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::ExitCode;

use coda_cli::CliError;
use coda_cli::autostart::connect_unchecked;
use coda_client::Client;
use coda_config::config::Dirs;
use coda_config::timing::Timing;
use coda_protocol::codec::{FrameDecoder, encode_frame};
use coda_store::{GrantKey, Store};
use common::{TestDaemon, cli, context, short_dir};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

const NO_GIT_MESSAGE: &str =
    "cannot grant outside a git repository; you will be prompted each time";

#[tokio::test]
async fn version_flag_prints_name_and_version() {
    // Arrange
    let dir = short_dir();
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "--version"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS);
    assert_eq!(
        outcome.out,
        concat!("coda ", env!("CARGO_PKG_VERSION"), "\n")
    );
}

#[tokio::test]
async fn ping_prints_pong_when_a_daemon_answers() {
    // Arrange
    let dir = short_dir();
    let daemon = TestDaemon::start(dir.path());
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "ping"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS);
    assert_eq!(outcome.out, "pong\n");
    daemon.stop().await;
}

#[tokio::test]
async fn status_prints_the_daemon_facts() {
    // Arrange
    let dir = short_dir();
    let daemon = TestDaemon::start(dir.path());
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "status"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS);
    for line in [
        format!("pid: {}", std::process::id()),
        "protocol: 1".to_owned(),
        "attached clients: 0".to_owned(),
        "active turns: 0".to_owned(),
        format!(
            "data dir: {}",
            Dirs::from_base(dir.path()).data_dir().display()
        ),
    ] {
        assert!(
            outcome.out.contains(&line),
            "missing {line:?} in {}",
            outcome.out
        );
    }
    daemon.stop().await;
}

#[tokio::test]
async fn status_and_ping_without_a_daemon_fail_with_a_clear_message() {
    for command in ["status", "ping"] {
        // Arrange
        let dir = short_dir();
        let ctx = context(dir.path());

        // Act
        let outcome = cli(&ctx, &["coda", command], "").await;

        // Assert
        assert_ne!(outcome.code, ExitCode::SUCCESS, "{command}");
        assert!(
            outcome.err.contains("no daemon is running"),
            "{command}: {}",
            outcome.err
        );
    }
}

#[tokio::test]
async fn bare_coda_with_a_running_daemon_reports_it() {
    // Arrange
    let dir = short_dir();
    let daemon = TestDaemon::start(dir.path());
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS);
    assert!(outcome.out.contains("daemon is running"), "{}", outcome.out);
    daemon.stop().await;
}

#[tokio::test]
async fn run_parses_its_flags_then_reports_it_is_not_available() {
    // Arrange
    let dir = short_dir();
    let ctx = context(dir.path());
    let args = [
        "coda",
        "run",
        "--prompt",
        "hi",
        "--cwd",
        "/x",
        "--mode",
        "plan",
        "--max-turns",
        "3",
        "--token-cap",
        "100",
    ];

    // Act
    let outcome = cli(&ctx, &args, "").await;

    // Assert
    assert_ne!(outcome.code, ExitCode::SUCCESS);
    assert!(outcome.err.contains("not yet available"), "{}", outcome.err);
}

#[tokio::test]
async fn run_with_an_unknown_mode_is_a_usage_error() {
    // Arrange
    let dir = short_dir();
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "run", "--mode", "chaos"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(2));
    assert!(outcome.err.contains("chaos"), "{}", outcome.err);
}

#[tokio::test]
async fn grant_add_stores_the_grant_through_the_daemon() {
    // Arrange
    let dir = short_dir();
    let daemon = TestDaemon::start(dir.path());
    let repo = dir.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join("sub")).unwrap();
    let canonical = fs::canonicalize(&repo).unwrap();
    let mut ctx = context(dir.path());
    ctx.cwd = repo.join("sub");

    // Act
    let outcome = cli(
        &ctx,
        &["coda", "grant", "add", "--repo", "..", "cargo test"],
        "",
    )
    .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let store = Store::open(&Dirs::from_base(dir.path()).data_dir()).unwrap();
    let repo_path = canonical.to_str().unwrap();
    let cwd = canonical.join("sub");
    let key = GrantKey {
        repo_path,
        tool: "bash",
        cwd: cwd.to_str().unwrap(),
        pattern: "cargo test",
    };
    assert!(store.lookup_grant(&key).unwrap().is_some());
    daemon.stop().await;
}

#[tokio::test]
async fn grant_add_outside_a_git_repository_fails_without_calling_the_daemon() {
    // Arrange
    let dir = short_dir();
    let plain = dir.path().join("plain");
    fs::create_dir_all(&plain).unwrap();
    let mut ctx = context(dir.path());
    ctx.exe = dir.path().join("no-such-binary");

    // Act
    let outcome = cli(&ctx, &["coda", "grant", "add", "--repo", "plain", "ls"], "").await;

    // Assert
    assert_ne!(outcome.code, ExitCode::SUCCESS);
    assert!(outcome.err.contains(NO_GIT_MESSAGE), "{}", outcome.err);
}

#[tokio::test]
async fn daemon_restart_refuses_while_other_clients_are_attached() {
    // Arrange
    let dir = short_dir();
    let daemon = TestDaemon::start(dir.path());
    let ctx = context(dir.path());
    let socket = Dirs::from_base(dir.path()).socket_path();
    let _attached = Client::connect(&socket, Timing::default()).await.unwrap();

    // Act
    let outcome = cli(&ctx, &["coda", "daemon", "restart"], "").await;

    // Assert
    assert_ne!(outcome.code, ExitCode::SUCCESS);
    assert!(outcome.err.contains("--force"), "{}", outcome.err);
    daemon.stop().await;
}

/// Answers `daemon.status` with an error and `daemon.shutdown` by going away.
fn serve_failing_status(base: &Path) {
    let socket = Dirs::from_base(base).socket_path();
    let run_dir = socket.parent().unwrap();
    fs::create_dir_all(run_dir).unwrap();
    fs::set_permissions(run_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut decoder = FrameDecoder::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).await.unwrap();
            decoder.push(&buf[..n]).unwrap();
            while let Some(frame) = decoder.next_frame().unwrap() {
                let request: Value = serde_json::from_slice(&frame).unwrap();
                let (id, method) = (&request["id"], request["method"].as_str().unwrap());
                let body = match method {
                    "daemon.status" => json!({"error": {"code": -32603, "message": "x"}}),
                    _ => json!({"result": {}}),
                };
                let mut reply = json!({"jsonrpc": "2.0", "id": id});
                reply
                    .as_object_mut()
                    .unwrap()
                    .extend(body.as_object().unwrap().clone());
                let frame = encode_frame(reply.to_string().as_bytes()).unwrap();
                stream.write_all(&frame).await.unwrap();
                if method == "daemon.shutdown" {
                    fs::remove_file(&socket).unwrap();
                    return;
                }
            }
        }
    });
}

#[tokio::test]
async fn daemon_restart_treats_a_failed_status_as_busy() {
    // Arrange
    let dir = short_dir();
    serve_failing_status(dir.path());
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "daemon", "restart"], "").await;

    // Assert
    assert_ne!(outcome.code, ExitCode::SUCCESS);
    assert!(outcome.err.contains("unknown activity"), "{}", outcome.err);
    assert!(outcome.err.contains("--force"), "{}", outcome.err);
}

#[tokio::test]
async fn forced_daemon_restart_stops_a_daemon_whose_status_fails() {
    // Arrange
    let dir = short_dir();
    serve_failing_status(dir.path());
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "--force", "daemon", "restart"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert!(matches!(
        connect_unchecked(&ctx).await,
        Err(CliError::NotRunning)
    ));
}
