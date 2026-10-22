// SPDX-License-Identifier: AGPL-3.0-only

//! The `[[hooks]]` table: what it accepts and what it refuses at load.

use std::time::Duration;

use coda_config::config::{Config, ConfigError};
use coda_config::hooks::{DEFAULT_TIMEOUT_MS, HookEvent, MAX_TIMEOUT_MS};

fn invalid_path(text: &str) -> String {
    match Config::from_toml_str(text).unwrap_err() {
        ConfigError::InvalidValue { path, .. } => path,
        other => panic!("expected an invalid value, got {other:?}"),
    }
}

#[test]
fn a_hook_has_an_event_a_program_with_arguments_and_defaults() {
    // Arrange
    let text = "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"/usr/bin/check\", \"--strict\"]\n\
                [[hooks]]\nevent = \"post_tool\"\ncommand = [\"log\"]\nblocking = true\n\
                timeout_ms = 250\n[[hooks]]\nevent = \"session_start\"\ncommand = [\"hello\"]\n";

    // Act
    let hooks = Config::from_toml_str(text).unwrap().config.hooks;

    // Assert
    assert_eq!(hooks.len(), 3);
    assert_eq!(hooks[0].event, HookEvent::PreTool);
    assert_eq!(hooks[0].command, ["/usr/bin/check", "--strict"]);
    assert!(!hooks[0].blocking);
    assert_eq!(
        hooks[0].timeout(),
        Duration::from_millis(DEFAULT_TIMEOUT_MS)
    );
    assert_eq!(hooks[1].event, HookEvent::PostTool);
    assert!(hooks[1].blocking);
    assert_eq!(hooks[1].timeout(), Duration::from_millis(250));
    assert_eq!(hooks[2].event, HookEvent::SessionStart);
}

#[test]
fn an_empty_command_or_program_is_rejected_with_the_hook_named() {
    for command in ["[]", "[\"\"]"] {
        // Arrange
        let text = format!(
            "[[hooks]]\nevent = \"session_start\"\ncommand = [\"ok\"]\n\
             [[hooks]]\nevent = \"pre_tool\"\ncommand = {command}\n"
        );

        // Act and Assert
        assert_eq!(invalid_path(&text), "hooks[1].command", "{command}");
    }
}

#[test]
fn a_timeout_of_zero_or_past_the_cap_is_rejected() {
    for timeout in [0, MAX_TIMEOUT_MS + 1] {
        // Arrange
        let text =
            format!("[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"x\"]\ntimeout_ms = {timeout}\n");

        // Act and Assert
        assert_eq!(invalid_path(&text), "hooks[0].timeout_ms", "{timeout}");
    }
    let at_cap = format!(
        "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"x\"]\ntimeout_ms = {MAX_TIMEOUT_MS}\n"
    );
    assert!(Config::from_toml_str(&at_cap).is_ok());
}

#[test]
fn a_shell_string_an_unknown_event_or_a_misspelled_key_is_a_syntax_error() {
    // A misspelled `blocking` must not silently turn a guard into a log line.
    let cases = [
        "event = \"pre_tool\"\ncommand = \"check --strict\"",
        "event = \"pre_tool_use\"\ncommand = [\"x\"]",
        "event = \"pre_tool\"\ncommand = [\"x\"]\nblockng = true",
    ];
    for case in cases {
        // Arrange
        let text = format!("[[hooks]]\n{case}\n");

        // Act
        let err = Config::from_toml_str(&text).unwrap_err();

        // Assert
        assert!(matches!(err, ConfigError::Syntax(_)), "{case}: {err:?}");
    }
}
