// SPDX-License-Identifier: Apache-2.0

//! The `env_passthrough` key of `[providers.<name>]`.

use coda_config::config::{Config, ConfigError};

fn section(list: &str) -> String {
    format!("[providers.claude_code]\nenabled = true\nenv_passthrough = {list}\n")
}

#[test]
fn plain_names_are_kept_in_order() {
    // Arrange
    let text = section(r#"["HTTPS_PROXY", "ANTHROPIC_API_KEY"]"#);

    // Act
    let config = Config::from_toml_str(&text).unwrap().config;

    // Assert
    assert_eq!(
        config.providers["claude_code"].env_passthrough,
        ["HTTPS_PROXY", "ANTHROPIC_API_KEY"]
    );
}

#[test]
fn a_name_that_is_not_a_variable_name_is_refused_naming_the_key() {
    for bad in [
        r#""""#,
        r#""A=B""#,
        r#""HAS SPACE""#,
        r#""1ABC""#,
        r#""ANTHROPIC_API_KEY=sk-x""#,
    ] {
        // Arrange
        let text = section(&format!("[{bad}]"));

        // Act
        let err = Config::from_toml_str(&text).unwrap_err();

        // Assert
        assert!(
            matches!(&err, ConfigError::InvalidValue { path, .. }
                if path == "providers.claude_code.env_passthrough"),
            "{bad}: {err}"
        );
    }
}

#[test]
fn a_refused_entry_never_echoes_its_value() {
    // Arrange
    let text = section(r#"["ANTHROPIC_API_KEY=sk-secret-value"]"#);

    // Act
    let err = Config::from_toml_str(&text).unwrap_err().to_string();

    // Assert
    assert!(!err.contains("sk-secret-value"), "{err}");
}
