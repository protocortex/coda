// SPDX-License-Identifier: Apache-2.0

//! The `[providers.<name>]` keys that shape a model request.

use coda_config::config::{Config, ConfigError, MaxTokensField, ProviderConfig};

fn section(body: &str) -> String {
    format!("[providers.local]\nenabled = true\n{body}")
}

fn anthropic_section(body: &str) -> String {
    format!("[providers.anthropic]\nenabled = true\n{body}")
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

#[test]
fn a_thinking_budget_is_parsed_and_unset_by_default() {
    // Arrange
    let set = section("thinking_budget_tokens = 2048\n");
    let unset = section("");

    // Act
    let set = Config::from_toml_str(&set).unwrap().config;
    let unset = Config::from_toml_str(&unset).unwrap().config;

    // Assert
    assert_eq!(set.providers["local"].thinking_budget_tokens, Some(2048));
    assert_eq!(unset.providers["local"].thinking_budget_tokens, None);
}

#[test]
fn a_thinking_budget_must_be_at_least_the_minimum_and_below_the_output_limit() {
    let cases = [
        "thinking_budget_tokens = 1023\n",
        "thinking_budget_tokens = 8192\n",
        "max_output_tokens = 4096\nthinking_budget_tokens = 4096\n",
    ];
    for body in cases {
        // Arrange
        let text = anthropic_section(body);

        // Act
        let err = Config::from_toml_str(&text).unwrap_err();

        // Assert
        assert!(
            matches!(&err, ConfigError::InvalidValue { path, .. }
                if path == "providers.anthropic.thinking_budget_tokens"),
            "{body}: {err}"
        );
    }
}

#[test]
fn a_thinking_budget_just_below_the_output_limit_is_accepted() {
    // Arrange
    let text = anthropic_section("max_output_tokens = 4096\nthinking_budget_tokens = 4095\n");

    // Act
    let config = Config::from_toml_str(&text).unwrap().config;

    // Assert
    assert_eq!(
        config.providers["anthropic"].thinking_budget_tokens,
        Some(4095)
    );
}

#[test]
fn a_thinking_budget_on_another_provider_is_not_checked() {
    // Arrange
    let text = section("thinking_budget_tokens = 5\n");

    // Act
    let config = Config::from_toml_str(&text).unwrap().config;

    // Assert
    assert_eq!(config.providers["local"].thinking_budget_tokens, Some(5));
}
