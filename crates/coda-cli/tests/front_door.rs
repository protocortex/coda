// SPDX-License-Identifier: AGPL-3.0-only

//! `coda` and `coda tui` choose between the TUI and plain output by an injected
//! terminal check, and `coda init` writes a starter config once.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::ExitCode;

use coda_cli::{TuiFuture, exit};
use coda_client::Client;
use coda_config::config::{Config, Dirs};
use coda_config::timing::Timing;
use coda_store::DB_FILE_NAME;
use coda_tui::Startup;
use coda_tui::commands::UserCommands;
use common::{StopDaemon, cli, context, short_dir};

const LAUNCH_MARKER: &str = "tui-launched";
const COMMANDS_MARKER: &str = "tui-commands";
const NO_TERMINAL_MESSAGE: &str = "needs an interactive terminal";
const FAUX_ENABLED: &str = "[providers.faux]\nenabled = true\nscript = \"/x/script.json\"\n";

/// Stands in for the TUI: checks the daemon answers on the socket it is given
/// and leaves a note of how it was started and which commands it was given.
fn record_launch(
    socket: &Path,
    timing: Timing,
    startup: Startup,
    commands: UserCommands,
) -> TuiFuture<'_> {
    Box::pin(async move {
        Client::connect(socket, timing)
            .await
            .expect("the daemon should answer before the TUI starts");
        let run_dir = socket.parent().unwrap();
        fs::write(
            run_dir.join(COMMANDS_MARKER),
            commands.help_lines().join("\n"),
        )
        .unwrap();
        fs::write(
            run_dir.join(LAUNCH_MARKER),
            format!(
                "needs_setup={} hooks_configured={}",
                startup.needs_setup, startup.hooks_configured
            ),
        )
        .unwrap();
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
        Some("needs_setup=true hooks_configured=false"),
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
        Some("needs_setup=false hooks_configured=false")
    );
}

#[tokio::test]
async fn a_config_with_hooks_tells_the_tui_so() {
    // Arrange
    let dir = short_dir();
    let _stop = StopDaemon::new(dir.path());
    let hooks = "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"/bin/true\"]\n";
    fs::write(
        Dirs::from_base(dir.path()).config_file(),
        format!("{hooks}{FAUX_ENABLED}"),
    )
    .unwrap();
    let mut ctx = context(dir.path());
    ctx.is_tty = true;
    ctx.launch_tui = record_launch;

    // Act
    let outcome = cli(&ctx, &["coda", "tui"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(
        launch_note(dir.path()).as_deref(),
        Some("needs_setup=false hooks_configured=true")
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

const REVIEW_COMMAND: &str = "+++\nname = \"review\"\ndescription = \"Reviews a change.\"\n\
    argument_hint = \"<change>\"\n+++\nReview $ARGUMENTS.\n";
const DEPLOY_COMMAND: &str = "+++\nname = \"deploy\"\ndescription = \"Ships it.\"\n+++\nDeploy.\n";

fn commands_note(base: &Path) -> String {
    let note = Dirs::from_base(base).run_dir().join(COMMANDS_MARKER);
    fs::read_to_string(note).expect("the TUI was not launched")
}

/// A home that is also a repository root, so project definitions are looked for in it.
fn repo_home() -> tempfile::TempDir {
    let dir = short_dir();
    fs::create_dir(dir.path().join(".git")).unwrap();
    dir
}

fn write_file(base: &Path, rel: &str, text: &str) {
    let path = base.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[tokio::test]
async fn the_tui_is_given_the_users_command_files() {
    // Arrange
    let dir = repo_home();
    let _stop = StopDaemon::new(dir.path());
    write_file(dir.path(), "commands/review.md", REVIEW_COMMAND);
    let mut ctx = context(dir.path());
    ctx.is_tty = true;
    ctx.launch_tui = record_launch;

    // Act
    let outcome = cli(&ctx, &["coda", "tui"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let note = commands_note(dir.path());
    assert!(
        note.contains("/review <change>: Reviews a change. (user)"),
        "{note}"
    );
}

#[tokio::test]
async fn the_tui_starts_with_a_warning_when_the_command_files_cannot_be_loaded() {
    // Arrange
    let dir = repo_home();
    let _stop = StopDaemon::new(dir.path());
    write_file(dir.path(), "commands/review.md", REVIEW_COMMAND);
    let mut ctx = context(dir.path());
    ctx.is_tty = true;
    ctx.launch_tui = record_launch;
    // The first start brings the daemon up, and it creates the store.
    cli(&ctx, &["coda", "tui"], "").await;
    let db = Dirs::from_base(dir.path()).data_dir().join(DB_FILE_NAME);
    fs::set_permissions(&db, fs::Permissions::from_mode(0o000)).unwrap();

    // Act
    let outcome = cli(&ctx, &["coda", "tui"], "").await;
    fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let note = commands_note(dir.path());
    assert!(note.contains("command files could not be loaded"), "{note}");
    assert!(!note.contains("/review"), "{note}");
}

#[tokio::test]
async fn a_project_command_is_pending_for_the_tui_until_the_repository_is_trusted() {
    // Arrange
    let dir = repo_home();
    let _stop = StopDaemon::new(dir.path());
    write_file(dir.path(), ".coda/commands/deploy.md", DEPLOY_COMMAND);
    let mut ctx = context(dir.path());
    ctx.is_tty = true;
    ctx.launch_tui = record_launch;

    // Act
    cli(&ctx, &["coda", "tui"], "").await;
    let before = commands_note(dir.path());
    cli(&ctx, &["coda", "agents", "trust"], "").await;
    cli(&ctx, &["coda", "tui"], "").await;
    let after = commands_note(dir.path());

    // Assert
    assert!(
        before.contains("/deploy: project command, pending"),
        "{before}"
    );
    assert!(after.contains("/deploy: Ships it. (project)"), "{after}");
}
