// SPDX-License-Identifier: AGPL-3.0-only

//! The `[routing]` table that maps agent tiers to model aliases.

use coda_config::config::{Config, ConfigError};
use coda_config::routing::Tier;

const MODELS: &str = "[models.small]\nprovider = \"faux\"\nmodel = \"m1\"\n\
                      [models.big]\nprovider = \"faux\"\nmodel = \"m2\"\n";

#[test]
fn a_tier_resolves_to_its_configured_alias() {
    // Arrange
    let text = format!("{MODELS}[routing]\nfast = \"small\"\ndeep = \"big\"\n");

    // Act
    let config = Config::from_toml_str(&text).unwrap().config;

    // Assert
    assert_eq!(config.model_alias_for(Tier::Fast), Some("small"));
    assert_eq!(config.model_alias_for(Tier::Deep), Some("big"));
}

#[test]
fn a_tier_with_no_entry_uses_the_default_model() {
    // Arrange
    let text = format!("{MODELS}[routing]\nfast = \"small\"\n");

    // Act
    let config = Config::from_toml_str(&text).unwrap().config;

    // Assert
    assert_eq!(config.model_alias_for(Tier::Standard), None);
}

#[test]
fn an_empty_config_routes_every_tier_to_the_default_model() {
    // Act
    let config = Config::default();

    // Assert
    for tier in Tier::ALL {
        assert_eq!(config.model_alias_for(tier), None);
    }
}

#[test]
fn a_route_to_an_unknown_alias_is_rejected_at_load() {
    // Arrange
    let text = format!("{MODELS}[routing]\nfast = \"missing\"\n");

    // Act
    let err = Config::from_toml_str(&text).unwrap_err();

    // Assert
    assert!(matches!(
        &err,
        ConfigError::InvalidValue { path, .. } if path == "routing.fast"
    ));
}

#[test]
fn an_unknown_tier_in_the_table_is_rejected_at_load() {
    // Arrange
    let text = format!("{MODELS}[routing]\nturbo = \"small\"\n");

    // Act
    let err = Config::from_toml_str(&text).unwrap_err();

    // Assert
    assert!(matches!(err, ConfigError::Syntax(_)));
}

#[test]
fn tier_names_parse_and_print_in_lowercase() {
    // Act and Assert
    for tier in Tier::ALL {
        assert_eq!(tier.as_str().parse::<Tier>(), Ok(tier));
    }
    assert!("Fast".parse::<Tier>().is_err());
}
