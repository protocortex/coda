// SPDX-License-Identifier: AGPL-3.0-only

//! The delegated-agent backend trait and its enable switch.

use std::path::PathBuf;

use coda_config::config::{Config, ProviderConfig};
use coda_protocol::types::BackendKind;

use crate::detect::{DEFAULT_PROBE_TIMEOUT, DetectRequest, DetectedBinary, Detection, detect};

/// Claude Code versions verified to work; extended by `verified_versions` in config.
pub const VERIFIED_CLAUDE_CODE_VERSIONS: &[&str] = &["2.1.292"];

/// Why a backend cannot be used.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BackendError {
    /// `providers.<backend>.enabled` is not set.
    #[error("backend {backend} is disabled; set providers.{backend}.enabled = true to use it")]
    Disabled {
        /// The config key of the backend.
        backend: String,
    },
    /// No executable was found.
    #[error("backend {backend} is enabled but no `{binary}` executable was found")]
    NotInstalled {
        /// The config key of the backend.
        backend: String,
        /// The file name that was searched for.
        binary: String,
    },
    /// A binary exists but did not report a version.
    #[error("backend {backend} binary {} is unusable: {reason}", .path.display())]
    Unusable {
        /// The config key of the backend.
        backend: String,
        /// The binary that was tried.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },
}

/// A delegated agent that coda supervises as a child process.
pub trait AgentBackend: Send + Sync {
    /// The protocol identity of this backend.
    fn kind(&self) -> BackendKind;
    /// The `[providers.<key>]` section that configures it.
    fn config_key(&self) -> &'static str;
    /// The executable's file name.
    fn binary_name(&self) -> &'static str;
    /// Versions verified to work, before any the user adds in config.
    fn verified_versions(&self) -> &'static [&'static str];
}

/// Returns the backend's config section if it is enabled.
///
/// # Errors
/// Returns [`BackendError::Disabled`] when the section is missing or
/// `enabled` is false.
pub fn require_enabled<'a>(
    backend: &dyn AgentBackend,
    config: &'a Config,
) -> Result<&'a ProviderConfig, BackendError> {
    config
        .providers
        .get(backend.config_key())
        .filter(|section| section.enabled)
        .ok_or_else(|| BackendError::Disabled {
            backend: backend.config_key().to_owned(),
        })
}

/// Finds the backend's binary once the backend is enabled.
///
/// `search_dirs` is the directory list to search, normally the split `PATH`,
/// passed in so callers and tests control it.
///
/// # Errors
/// Returns [`BackendError::Disabled`] before looking at the filesystem when
/// the backend is not enabled, [`BackendError::NotInstalled`] when no
/// executable is found, and [`BackendError::Unusable`] when it reports no version.
pub fn locate(
    backend: &dyn AgentBackend,
    config: &Config,
    search_dirs: &[PathBuf],
) -> Result<DetectedBinary, BackendError> {
    let section = require_enabled(backend, config)?;
    let verified: Vec<String> = backend
        .verified_versions()
        .iter()
        .map(|version| (*version).to_owned())
        .chain(section.verified_versions.iter().cloned())
        .collect();
    let request = DetectRequest {
        binary_name: backend.binary_name(),
        explicit: section.binary.as_deref(),
        search_dirs,
        verified: &verified,
        probe_timeout: DEFAULT_PROBE_TIMEOUT,
    };
    match detect(&request) {
        Detection::Found(binary) => Ok(binary),
        Detection::Absent => Err(BackendError::NotInstalled {
            backend: backend.config_key().to_owned(),
            binary: backend.binary_name().to_owned(),
        }),
        Detection::Unusable { path, reason } => Err(BackendError::Unusable {
            backend: backend.config_key().to_owned(),
            path,
            reason,
        }),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct Claude;

    impl AgentBackend for Claude {
        fn kind(&self) -> BackendKind {
            BackendKind::ClaudeCode
        }
        fn config_key(&self) -> &'static str {
            "claude_code"
        }
        fn binary_name(&self) -> &'static str {
            "claude"
        }
        fn verified_versions(&self) -> &'static [&'static str] {
            VERIFIED_CLAUDE_CODE_VERSIONS
        }
    }

    fn config(toml: &str) -> Config {
        Config::from_toml_str(toml).unwrap().config
    }

    fn dir_with_claude(version: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude");
        fs::write(
            &path,
            format!("#!/bin/sh\necho '{version} (Claude Code)'\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    #[test]
    fn a_backend_without_a_config_section_is_disabled() {
        // Arrange
        let config = config("");

        // Act
        let result = require_enabled(&Claude, &config);

        // Assert
        assert_eq!(
            result.unwrap_err(),
            BackendError::Disabled {
                backend: "claude_code".to_owned()
            }
        );
    }

    #[test]
    fn a_backend_with_enabled_false_is_disabled() {
        // Arrange
        let config = config("[providers.claude_code]\nenabled = false\n");

        // Act
        let result = require_enabled(&Claude, &config);

        // Assert
        assert!(matches!(result, Err(BackendError::Disabled { .. })));
    }

    #[test]
    fn enabling_the_backend_returns_its_section() {
        // Arrange
        let config = config("[providers.claude_code]\nenabled = true\n");

        // Act
        let section = require_enabled(&Claude, &config).unwrap();

        // Assert
        assert!(section.enabled);
    }

    #[test]
    fn a_disabled_backend_is_not_searched_for() {
        // Arrange
        let dir = dir_with_claude("2.1.292");
        let config = config("");

        // Act
        let result = locate(&Claude, &config, &[dir.path().to_path_buf()]);

        // Assert
        assert!(matches!(result, Err(BackendError::Disabled { .. })));
    }

    #[test]
    fn an_enabled_backend_is_located_and_verified_against_the_constant() {
        // Arrange
        let dir = dir_with_claude("2.1.292");
        let config = config("[providers.claude_code]\nenabled = true\n");

        // Act
        let binary = locate(&Claude, &config, &[dir.path().to_path_buf()]).unwrap();

        // Assert
        assert_eq!(binary.version, "2.1.292");
        assert!(binary.verified);
    }

    #[test]
    fn config_verified_versions_extend_the_constant() {
        // Arrange
        let dir = dir_with_claude("2.5.0");
        let config =
            config("[providers.claude_code]\nenabled = true\nverified_versions = [\"2.5.0\"]\n");

        // Act
        let binary = locate(&Claude, &config, &[dir.path().to_path_buf()]).unwrap();

        // Assert
        assert!(binary.verified);
    }

    #[test]
    fn an_unlisted_version_is_located_but_not_verified() {
        // Arrange
        let dir = dir_with_claude("0.0.1");
        let config = config("[providers.claude_code]\nenabled = true\n");

        // Act
        let binary = locate(&Claude, &config, &[dir.path().to_path_buf()]).unwrap();

        // Assert
        assert!(!binary.verified);
    }

    #[test]
    fn a_configured_binary_path_is_used_instead_of_the_search() {
        // Arrange
        let dir = dir_with_claude("2.1.292");
        let toml = format!(
            "[providers.claude_code]\nenabled = true\nbinary = \"{}\"\n",
            dir.path().join("claude").display()
        );
        let config = config(&toml);

        // Act
        let binary = locate(&Claude, &config, &[]).unwrap();

        // Assert
        assert_eq!(binary.path, dir.path().join("claude"));
    }

    #[test]
    fn an_enabled_backend_with_no_binary_is_not_installed() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let config = config("[providers.claude_code]\nenabled = true\n");

        // Act
        let result = locate(&Claude, &config, &[dir.path().to_path_buf()]);

        // Assert
        assert_eq!(
            result.unwrap_err(),
            BackendError::NotInstalled {
                backend: "claude_code".to_owned(),
                binary: "claude".to_owned()
            }
        );
    }
}
