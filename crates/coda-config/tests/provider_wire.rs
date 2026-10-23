// SPDX-License-Identifier: AGPL-3.0-only

//! The `[providers.<name>]` keys that shape a model request.

use coda_config::config::{Config, ConfigError, MaxTokensField, ProviderConfig};

fn section(body: &str) -> String {
    format!("[providers.local]\nenabled = true\n{body}")
}

#[test]
fn unset_keys_keep_the_built_in_wire_shape() {
    // Arrange
    let text = section("");

    // Act
    let config = Config::from_toml_str(&text).unwrap().config;

    // Assert
    let local = &config.providers["local"];
    assert_eq!(local.max_output_tokens, None);
    assert_eq!(local.max_tokens_field, MaxTokensField::MaxTokens);
    assert!(local.stream_usage, "unset stream_usage means on");
    assert!(ProviderConfig::default().stream_usage);
}

#[test]
fn each_key_is_parsed() {
    // Arrange
    let text = section(
        "max_output_tokens = 2048\nmax_tokens_field = \"max_completion_tokens\"\nstream_usage = false\n",
    );

    // Act
    let parsed = Config::from_toml_str(&text).unwrap();

    // Assert
    let local = &parsed.config.providers["local"];
    assert_eq!(local.max_output_tokens, Some(2048));
    assert_eq!(local.max_tokens_field, MaxTokensField::MaxCompletionTokens);
    assert!(!local.stream_usage);
    assert_eq!(parsed.unknown_keys, Vec::<String>::new());
}

#[test]
fn the_field_names_in_the_file_are_the_ones_sent() {
    let names = [
        ("max_tokens", MaxTokensField::MaxTokens),
        ("max_completion_tokens", MaxTokensField::MaxCompletionTokens),
    ];
    for (name, field) in names {
        // Arrange
        let text = section(&format!("max_tokens_field = \"{name}\"\n"));

        // Act
        let config = Config::from_toml_str(&text).unwrap().config;

        // Assert
        assert_eq!(config.providers["local"].max_tokens_field, field);
        assert_eq!(field.as_str(), name);
    }
}

#[test]
fn an_unknown_max_tokens_field_is_refused_naming_the_key() {
    for value in ["\"max_token\"", "7"] {
        // Arrange
        let text = section(&format!("max_tokens_field = {value}\n"));

        // Act
        let err = Config::from_toml_str(&text).unwrap_err();

        // Assert
        assert!(
            err.to_string().contains("providers.local.max_tokens_field"),
            "{err}"
        );
    }
}

#[test]
fn a_zero_output_limit_is_refused_naming_the_key() {
    // Arrange
    let text = section("max_output_tokens = 0\n");

    // Act
    let err = Config::from_toml_str(&text).unwrap_err();

    // Assert
    assert!(
        matches!(&err, ConfigError::InvalidValue { path, .. }
            if path == "providers.local.max_output_tokens"),
        "{err}"
    );
}
