// SPDX-License-Identifier: Apache-2.0

//! The `profile` key of `[providers.claude_code]`.

use coda_config::config::{ClaudeProfile, Config, ProviderConfig};

fn section(body: &str) -> String {
    format!("[providers.claude_code]\nenabled = true\n{body}")
}

#[test]
fn an_unset_profile_is_project() {
    // Arrange
    let text = section("");

    // Act
    let config = Config::from_toml_str(&text).unwrap().config;

    // Assert
    assert_eq!(
        config.providers["claude_code"].profile,
        ClaudeProfile::Project
    );
    assert_eq!(ProviderConfig::default().profile, ClaudeProfile::Project);
}

#[test]
fn each_profile_name_is_parsed() {
    let names = [
        ("full", ClaudeProfile::Full),
        ("project", ClaudeProfile::Project),
        ("lean", ClaudeProfile::Lean),
    ];
    for (name, profile) in names {
        // Arrange
        let text = section(&format!("profile = \"{name}\"\n"));

        // Act
        let config = Config::from_toml_str(&text).unwrap().config;

        // Assert
        assert_eq!(config.providers["claude_code"].profile, profile);
        assert_eq!(profile.as_str(), name);
    }
}

#[test]
fn an_unknown_profile_is_refused_naming_the_key() {
    for value in ["\"minimal\"", "3"] {
        // Arrange
        let text = section(&format!("profile = {value}\n"));

        // Act
        let err = Config::from_toml_str(&text).unwrap_err();

        // Assert
        assert!(
            err.to_string().contains("providers.claude_code.profile"),
            "{err}"
        );
    }
}
