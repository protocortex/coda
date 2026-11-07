// SPDX-License-Identifier: Apache-2.0

//! `coda login`: hands the login to the vendor CLI, then asks again.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use coda_config::config::Dirs;
use common::{cli, context, short_dir};

const FAILURE: u8 = 70;
const LOGGED_IN: &str = r#"{"loggedIn":true,"authMethod":"claude.ai","subscriptionType":"max"}"#;
const LOGGED_OUT: &str = r#"{"loggedIn":false,"authMethod":"none"}"#;

/// A `claude` stand-in. `auth login` writes a marker file (and exits `login_exit`), and
/// `auth status` reports a login only once the marker exists, or always when `always_in`.
fn fake_claude(dir: &Path, login_exit: u8, always_in: bool) -> PathBuf {
    let marker = dir.join("logged-in");
    let log = dir.join("calls");
    let status = if always_in {
        format!("echo '{LOGGED_IN}'")
    } else {
        format!(
            "if [ -e '{}' ]; then echo '{LOGGED_IN}'; else echo '{LOGGED_OUT}'; exit 1; fi",
            marker.display()
        )
    };
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> '{log}'\ncase \"$1\" in\n--version) echo '2.1.296 (Claude Code)';;\nauth) case \"$2\" in\nlogin) touch '{marker}'; exit {login_exit};;\nstatus) {status};;\nesac;;\nesac\n",
        log = log.display(),
        marker = marker.display(),
    );
    let path = dir.join("claude");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn calls(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn home_with(claude: &Path) -> tempfile::TempDir {
    let home = short_dir();
    let config = format!(
        "[providers.claude_code]\nenabled = true\nbinary = {:?}\nverified_versions = [\"2.1.296\"]\n",
        claude.display().to_string()
    );
    fs::write(Dirs::from_base(home.path()).config_file(), config).unwrap();
    home
}

#[tokio::test]
async fn a_logged_out_backend_is_logged_in_by_its_own_cli_after_a_yes() {
    // Arrange
    let bin = short_dir();
    let claude = fake_claude(bin.path(), 0, false);
    let home = home_with(&claude);

    // Act
    let outcome = cli(
        &context(home.path()),
        &["coda", "login", "claude-code"],
        "y\n",
    )
    .await;

    // Assert
    assert_eq!(
        outcome.code,
        ExitCode::SUCCESS,
        "{}{}",
        outcome.out,
        outcome.err
    );
    assert!(outcome.out.contains("claude auth login"), "{}", outcome.out);
    assert!(outcome.out.contains("logged in"), "{}", outcome.out);
    assert!(calls(bin.path()).contains(&"auth login".to_owned()));
}

#[tokio::test]
async fn anything_but_a_yes_leaves_the_login_alone() {
    // Arrange
    let bin = short_dir();
    let claude = fake_claude(bin.path(), 0, false);
    let home = home_with(&claude);

    // Act
    let outcome = cli(
        &context(home.path()),
        &["coda", "login", "claude-code"],
        "\n",
    )
    .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(FAILURE));
    assert!(!calls(bin.path()).contains(&"auth login".to_owned()));
    assert!(outcome.out.contains("[y/N]"), "{}", outcome.out);
}

#[tokio::test]
async fn yes_skips_the_question() {
    // Arrange
    let bin = short_dir();
    let claude = fake_claude(bin.path(), 0, false);
    let home = home_with(&claude);

    // Act
    let outcome = cli(
        &context(home.path()),
        &["coda", "login", "claude-code", "--yes"],
        "",
    )
    .await;

    // Assert
    assert_eq!(
        outcome.code,
        ExitCode::SUCCESS,
        "{}{}",
        outcome.out,
        outcome.err
    );
    assert!(!outcome.out.contains("[y/N]"));
    assert!(calls(bin.path()).contains(&"auth login".to_owned()));
}

#[tokio::test]
async fn a_backend_that_is_already_logged_in_is_not_asked_to_log_in_again() {
    // Arrange
    let bin = short_dir();
    let claude = fake_claude(bin.path(), 0, true);
    let home = home_with(&claude);

    // Act
    let outcome = cli(
        &context(home.path()),
        &["coda", "login", "claude-code"],
        "y\n",
    )
    .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS);
    assert!(outcome.out.contains("already logged in"), "{}", outcome.out);
    assert!(!calls(bin.path()).contains(&"auth login".to_owned()));
}

#[tokio::test]
async fn an_api_key_login_is_named_and_not_switched() {
    // Arrange
    let bin = short_dir();
    let claude = fake_claude(bin.path(), 0, true);
    let script = fs::read_to_string(&claude).unwrap().replace(
        LOGGED_IN,
        r#"{"loggedIn":true,"authMethod":"api_key","apiKeySource":"user"}"#,
    );
    fs::write(&claude, script).unwrap();
    let home = home_with(&claude);

    // Act
    let outcome = cli(
        &context(home.path()),
        &["coda", "login", "claude-code"],
        "y\n",
    )
    .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS);
    assert!(outcome.out.contains("API key"), "{}", outcome.out);
    assert!(outcome.out.contains("does not switch"), "{}", outcome.out);
    assert!(!calls(bin.path()).contains(&"auth login".to_owned()));
}

#[tokio::test]
async fn a_login_that_fails_or_changes_nothing_is_a_failure() {
    for (login_exit, expected) in [(1_u8, "exited"), (0, "CLAUDE_CONFIG_DIR")] {
        // Arrange
        let bin = short_dir();
        let claude = fake_claude(bin.path(), login_exit, false);
        if login_exit == 0 {
            // `auth login` succeeds but the status never changes.
            let script = fs::read_to_string(&claude)
                .unwrap()
                .replace("touch", "true");
            fs::write(&claude, script).unwrap();
        }
        let home = home_with(&claude);

        // Act
        let outcome = cli(
            &context(home.path()),
            &["coda", "login", "claude-code", "--yes"],
            "",
        )
        .await;

        // Assert
        assert_eq!(outcome.code, ExitCode::from(FAILURE), "{expected}");
        let said = format!("{}{}", outcome.out, outcome.err);
        assert!(said.contains(expected), "{expected}: {said}");
    }
}

#[tokio::test]
async fn a_backend_that_is_not_enabled_cannot_log_in() {
    // Arrange
    let home = short_dir();

    // Act
    let outcome = cli(
        &context(home.path()),
        &["coda", "login", "claude-code", "--yes"],
        "",
    )
    .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(FAILURE));
    assert!(outcome.err.contains("claude_code"), "{}", outcome.err);
}
