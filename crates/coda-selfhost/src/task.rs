// SPDX-License-Identifier: AGPL-3.0-only

//! The pinned task and the scripted faux provider that solves it.

use serde_json::{Value, json};

/// What every attempt is asked to do.
pub const PROMPT: &str = "Add a `coda version --json` subcommand to crates/coda-cli/src/main.rs. \
`fn main` there only delegates to run(), so add the check inside `fn main`, before clap parses \
the arguments: when the arguments are exactly `version --json`, print the JSON and return \
success. Do not edit cli.rs or lib.rs. The JSON object has the keys name and version, taken from \
env!(\"CARGO_PKG_NAME\") and env!(\"CARGO_PKG_VERSION\") and formatted by hand, so Cargo.toml \
needs no change. Add a test for it to crates/coda-cli/tests/version.rs. Then run exactly \
`cargo test` in the foreground, not in the background, and wait for it to finish before you \
answer.";

/// `[permissions.grants]` entries a run is pre-approved for.
pub const GRANTS: [&str; 3] = [
    "bash:cargo test",
    "bash:cargo build",
    "edit:crates/coda-cli/src/main.rs",
];

/// Caps on one attempt.
pub const MAX_TURNS: u32 = 30;
pub const TOKEN_CAP: u64 = 400_000;

/// The text each pinned edit replaces and what it is replaced with.
pub const MAIN_OLD: &str = "fn main() -> ExitCode {\n";
pub const MAIN_NEW: &str = r##"/// Whether the arguments are exactly `coda version --json`.
fn is_version_json(args: &[std::ffi::OsString]) -> bool {
    matches!(args, [_, version, flag] if version == "version" && flag == "--json")
}

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if is_version_json(&args) {
        println!(
            r#"{{"name":"{}","version":"{}"}}"#,
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        );
        return ExitCode::SUCCESS;
    }
"##;
pub const TEST_OLD: &str = r#"        assert_eq!(String::from_utf8_lossy(&output.stdout), expected, "{flag}");
    }
}
"#;
pub const TEST_NEW: &str = r##"        assert_eq!(String::from_utf8_lossy(&output.stdout), expected, "{flag}");
    }
}

#[test]
fn version_json_prints_name_and_version_as_json() {
    // Arrange
    let expected = format!(
        r#"{{"name":"{}","version":"{}"}}"#,
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    // Act
    let output = Command::new(env!("CARGO_BIN_EXE_coda"))
        .args(["version", "--json"])
        .output()
        .expect("coda binary should start");

    // Assert
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim_end(), expected);
}
"##;

/// The faux provider script that makes the pinned change.
#[must_use]
pub fn faux_script() -> String {
    script(&[
        edit_turn("c1", "crates/coda-cli/src/main.rs", MAIN_OLD, MAIN_NEW),
        edit_turn("c2", "crates/coda-cli/tests/version.rs", TEST_OLD, TEST_NEW),
        text_turn("The version subcommand and its test are in place."),
    ])
}

/// A faux provider script whose one edit targets an execution-affecting path.
#[must_use]
pub fn manifest_edit_script() -> String {
    script(&[
        edit_turn("c1", "Cargo.toml", "resolver = \"3\"", "resolver = \"2\""),
        text_turn("The manifest edit was refused."),
    ])
}

fn script(turns: &[Value]) -> String {
    Value::Array(turns.to_vec()).to_string()
}

fn edit_turn(call_id: &str, path: &str, old: &str, new: &str) -> Value {
    let arguments = json!({"path": path, "old_string": old, "new_string": new});
    json!({"events": [
        {"type": "tool_call_start", "call_id": call_id, "name": "edit"},
        {"type": "tool_call_arguments_delta", "call_id": call_id, "fragment": arguments.to_string()},
        {"type": "usage", "input_tokens": 5, "output_tokens": 3},
        {"type": "stop", "reason": "tool_use"},
    ]})
}

fn text_turn(text: &str) -> Value {
    json!({"events": [
        {"type": "text_delta", "text": text},
        {"type": "usage", "input_tokens": 5, "output_tokens": 3},
        {"type": "stop", "reason": "end_turn"},
    ]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_pins_the_edit_to_main_before_argument_parsing() {
        // Act
        let prompt = PROMPT;

        // Assert
        assert!(prompt.contains("`fn main`"), "names the function to edit");
        assert!(
            prompt.contains("before clap"),
            "main only delegates to run(), so the model must be told where the check goes"
        );
        assert!(
            prompt.contains("Do not edit cli.rs or lib.rs"),
            "the model must not move the change out of main.rs"
        );
    }

    #[test]
    fn the_prompt_asks_for_a_foreground_test_run_the_grants_cover() {
        // Act
        let prompt = PROMPT;

        // Assert
        assert!(prompt.contains("exactly `cargo test`"));
        assert!(prompt.contains("in the foreground"));
        assert!(GRANTS.contains(&"bash:cargo test"));
    }
}
