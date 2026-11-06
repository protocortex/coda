// SPDX-License-Identifier: Apache-2.0

//! The `[checks]` table: what it accepts and what it refuses at load.

use std::time::Duration;

use coda_config::checks::DEFAULT_TIMEOUT_MS;
use coda_config::config::{Config, ConfigError};
use coda_config::hooks::MAX_TIMEOUT_MS;

fn invalid_path(text: &str) -> String {
    match Config::from_toml_str(text).unwrap_err() {
        ConfigError::InvalidValue { path, .. } => path,
        other => panic!("expected an invalid value, got {other:?}"),
    }
}

#[test]
fn no_checks_are_set_by_default() {
    // Act
    let config = Config::from_toml_str("").unwrap().config;

    // Assert
    assert!(config.checks.is_empty());
}

#[test]
fn a_check_is_a_command_per_extension_with_a_default_timeout() {
    // Arrange
    let text = "[checks.rs]\ncommand = [\"cargo\", \"check\", \"--quiet\"]\n\
                [checks.py]\ncommand = [\"/usr/bin/ruff\", \"check\"]\ntimeout_ms = 2500\n";

    // Act
    let checks = Config::from_toml_str(text).unwrap().config.checks;

    // Assert
    assert_eq!(checks.len(), 2);
    assert_eq!(checks["rs"].command, ["cargo", "check", "--quiet"]);
    assert_eq!(
        checks["rs"].timeout(),
        Duration::from_millis(DEFAULT_TIMEOUT_MS)
    );
    assert_eq!(checks["py"].timeout(), Duration::from_millis(2500));
}

#[test]
fn an_empty_command_or_program_is_rejected_with_the_extension_named() {
    for command in ["[]", "[\"\"]"] {
        // Arrange
        let text = format!("[checks.rs]\ncommand = {command}\n");

        // Act
        let path = invalid_path(&text);

        // Assert
        assert_eq!(path, "checks.rs.command");
    }
}

#[test]
fn a_relative_program_path_is_rejected() {
    // Arrange
    let text = "[checks.rs]\ncommand = [\"./check.sh\"]\n";

    // Act
    let path = invalid_path(text);

    // Assert
    assert_eq!(path, "checks.rs.command");
}

#[test]
fn a_timeout_of_zero_or_over_the_cap_is_rejected() {
    for ms in [0, MAX_TIMEOUT_MS + 1] {
        // Arrange
        let text = format!("[checks.rs]\ncommand = [\"check\"]\ntimeout_ms = {ms}\n");

        // Act
        let path = invalid_path(&text);

        // Assert
        assert_eq!(path, "checks.rs.timeout_ms");
    }
}

#[test]
fn an_extension_with_a_dot_or_a_separator_is_rejected() {
    for (name, shown) in [
        ("\".rs\"", ".rs"),
        ("\"a/b\"", "a/b"),
        ("\"\"", ""),
        ("\"r s\"", "r s"),
    ] {
        // Arrange
        let text = format!("[checks.{name}]\ncommand = [\"check\"]\n");

        // Act
        let path = invalid_path(&text);

        // Assert
        assert_eq!(path, format!("checks.{shown}"));
    }
}

#[test]
fn two_extensions_that_differ_only_in_case_are_rejected() {
    // Arrange
    let text = "[checks.rs]\ncommand = [\"a\"]\n[checks.RS]\ncommand = [\"b\"]\n";

    // Act
    let path = invalid_path(text);

    // Assert
    assert_eq!(path, "checks.rs");
}
