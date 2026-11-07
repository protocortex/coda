// SPDX-License-Identifier: Apache-2.0

//! `coda login`: hands the login to the vendor CLI, then asks again.

mod common;

use std::fs;
use std::process::ExitCode;

use common::{LOGGED_IN, calls, cli, context, fake_claude, home_with, short_dir};

const FAILURE: u8 = 70;

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
