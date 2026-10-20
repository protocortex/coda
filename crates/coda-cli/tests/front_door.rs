// SPDX-License-Identifier: AGPL-3.0-only

//! `coda` and `coda tui` choose between the TUI and plain output by an injected
//! terminal check, and `coda init` writes a starter config once.

mod common;

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use coda_cli::{TuiFuture, exit};
use coda_client::Client;
use coda_config::config::{Config, Dirs};
use coda_config::timing::Timing;
use common::{StopDaemon, cli, context, short_dir};

const LAUNCH_MARKER: &str = "tui-launched";
const NO_TERMINAL_MESSAGE: &str = "needs an interactive terminal";
const FAUX_ENABLED: &str = "[providers.faux]\nenabled = true\nscript = \"/x/script.json\"\n";

/// Stands in for the TUI: checks the daemon answers on the socket it is given
/// and leaves a note of how it was started.
fn record_launch(socket: &Path, timing: Timing, needs_setup: bool) -> TuiFuture<'_> {
    Box::pin(async move {
        Client::connect(socket, timing)
            .await
            .expect("the daemon should answer before the TUI starts");
        let note = socket.parent().unwrap().join(LAUNCH_MARKER);
        fs::write(note, format!("needs_setup={needs_setup}")).unwrap();
        Ok(())
    })
}

fn launch_note(base: &Path) -> Option<String> {
    let note = Dirs::from_base(base).run_dir().join(LAUNCH_MARKER);
    fs::read_to_string(note).ok()
}

#[tokio::test]
async fn bare_coda_on_a_terminal_starts_the_daemon_and_runs_the_tui() {
    // Arrange
    let dir = short_dir();
    let _stop = StopDaemon::new(dir.path());
    let mut ctx = context(dir.path());
    ctx.is_tty = true;
    ctx.launch_tui = record_launch;

    // Act
    let outcome = cli(&ctx, &["coda"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(
        launch_note(dir.path()).as_deref(),
        Some("needs_setup=true"),
        "no provider is enabled in a fresh home"
    );
}

#[tokio::test]
async fn bare_coda_without_a_terminal_prints_the_status_as_before() {
    // Arrange
    let dir = short_dir();
    let _stop = StopDaemon::new(dir.path());
    let mut ctx = context(dir.path());
    ctx.launch_tui = record_launch;

    // Act
    let outcome = cli(&ctx, &["coda"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert!(outcome.out.starts_with("daemon is running (pid "));
    assert_eq!(launch_note(dir.path()), None);
}

#[tokio::test]
async fn coda_tui_on_a_terminal_runs_the_tui() {
    // Arrange
    let dir = short_dir();
    let _stop = StopDaemon::new(dir.path());
    let mut ctx = context(dir.path());
    ctx.is_tty = true;
    ctx.launch_tui = record_launch;

    // Act
    let outcome = cli(&ctx, &["coda", "tui"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert!(launch_note(dir.path()).is_some());
}

#[tokio::test]
async fn coda_tui_without_a_terminal_fails_and_says_why() {
    // Arrange
    let dir = short_dir();
    let mut ctx = context(dir.path());
    ctx.launch_tui = record_launch;

    // Act
    let outcome = cli(&ctx, &["coda", "tui"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(exit::FAILURE));
    assert!(outcome.err.contains(NO_TERMINAL_MESSAGE), "{}", outcome.err);
    assert_eq!(launch_note(dir.path()), None);
}

#[tokio::test]
async fn an_enabled_provider_means_the_tui_needs_no_setup() {
    // Arrange
    let dir = short_dir();
    let _stop = StopDaemon::new(dir.path());
    fs::write(Dirs::from_base(dir.path()).config_file(), FAUX_ENABLED).unwrap();
    let mut ctx = context(dir.path());
    ctx.is_tty = true;
    ctx.launch_tui = record_launch;

    // Act
    let outcome = cli(&ctx, &["coda", "tui"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(
        launch_note(dir.path()).as_deref(),
        Some("needs_setup=false")
    );
}

#[tokio::test]
async fn init_writes_a_starter_config_that_parses_with_no_provider_enabled() {
    // Arrange
    let dir = short_dir();
    let ctx = context(dir.path());
    let config_file = Dirs::from_base(dir.path()).config_file();

    // Act
    let outcome = cli(&ctx, &["coda", "init"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert!(outcome.out.contains(&config_file.display().to_string()));
    assert!(
        outcome.out.contains("coda daemon restart"),
        "{}",
        outcome.out
    );
    let parsed = Config::load(&config_file).unwrap();
    assert_eq!(parsed.unknown_keys, Vec::<String>::new());
    assert!(parsed.config.providers.values().all(|p| !p.enabled));
    let text = fs::read_to_string(&config_file).unwrap();
    for name in ["claude_code", "codex", "anthropic", "openai_compat", "faux"] {
        assert!(text.contains(&format!("[providers.{name}]")), "{name}");
    }
}

#[tokio::test]
async fn init_refuses_to_overwrite_an_existing_config() {
    // Arrange
    let dir = short_dir();
    let ctx = context(dir.path());
    let config_file = Dirs::from_base(dir.path()).config_file();
    fs::write(&config_file, FAUX_ENABLED).unwrap();

    // Act
    let outcome = cli(&ctx, &["coda", "init"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(exit::FAILURE));
    assert!(outcome.err.contains("already exists"), "{}", outcome.err);
    assert_eq!(fs::read_to_string(&config_file).unwrap(), FAUX_ENABLED);
}
