// SPDX-License-Identifier: AGPL-3.0-only

//! The `config.toml` model, its parser, and the config/data directory layout.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::timing::Timing;

const DEFAULT_MAX_TURNS: u32 = 50;
const DEFAULT_TOKEN_CAP: u64 = 1_000_000;

/// Environment variable that moves the config and data root.
pub const CODA_HOME_VAR: &str = "CODA_HOME";
const HOME_VAR: &str = "HOME";
const DEFAULT_BASE_UNDER_HOME: &str = ".config/coda";
const CONFIG_FILE_NAME: &str = "config.toml";
const DATA_DIR_NAME: &str = "data";
const RUN_DIR_NAME: &str = "run";
const SOCKET_FILE_NAME: &str = "coda.sock";

const PROVIDERS_KEY: &str = "providers";
const SECRET_KEY_SUFFIXES: [&str; 7] = [
    "api_key",
    "apikey",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
];
const ENV_NAME_SUFFIX: &str = "_env";

/// Why reading or parsing the configuration failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The config file could not be read.
    #[error("cannot read config file {}: {source}", path.display())]
    Io {
        /// File that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The config file is not valid for the schema.
    #[error("invalid config file {}: {source}", path.display())]
    Parse {
        /// File that failed to parse.
        path: PathBuf,
        /// Underlying TOML error.
        source: toml::de::Error,
    },
    /// The config text is not valid for the schema.
    #[error("invalid config: {0}")]
    Syntax(#[from] toml::de::Error),
    /// A provider section holds a literal secret instead of an env var name.
    #[error(
        "plaintext secret at `{path}`: store the key in an environment variable and use `api_key_env`"
    )]
    PlaintextSecret {
        /// Dotted path of the offending key.
        path: String,
    },
    /// Neither `CODA_HOME` nor `HOME` is set.
    #[error("cannot locate the config directory: neither CODA_HOME nor HOME is set")]
    NoHomeDir,
}

/// How tool permission requests are answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Ask the user for each request.
    #[default]
    Prompt,
    /// Refuse every request.
    DenyAll,
    /// Allow file edits, ask for the rest.
    AcceptEdits,
}

/// The `[permissions]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Permissions {
    /// Answering policy.
    pub mode: PermissionMode,
    /// Pre-approved grants.
    pub grants: Vec<String>,
}

/// One `[providers.<backend>]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Whether the backend may be used.
    pub enabled: bool,
    /// Explicit path of the backend binary; searched on the path when unset.
    pub binary: Option<PathBuf>,
    /// Name of the environment variable holding the API key.
    pub api_key_env: Option<String>,
    /// Extra environment variables forwarded to the backend process.
    pub env_passthrough: Vec<String>,
    /// Backend versions known to work.
    pub verified_versions: Vec<String>,
    /// Script file replayed by the `faux` backend.
    pub script: Option<PathBuf>,
}

/// The `[telemetry]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// OTLP collector endpoint, when export is wanted.
    pub otlp_endpoint: Option<String>,
    /// Whether prompt and response content is recorded.
    pub capture_content: bool,
}

/// The whole configuration; every field has a default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Permission policy.
    pub permissions: Permissions,
    /// Maximum agent turns per run.
    pub max_turns: u32,
    /// Maximum tokens per run.
    pub token_cap: u64,
    /// Backends by name.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Telemetry settings.
    pub telemetry: TelemetryConfig,
    /// Timeout overrides.
    pub timing: Timing,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            permissions: Permissions::default(),
            max_turns: DEFAULT_MAX_TURNS,
            token_cap: DEFAULT_TOKEN_CAP,
            providers: BTreeMap::new(),
            telemetry: TelemetryConfig::default(),
            timing: Timing::default(),
        }
    }
}

/// A parsed config plus the keys the schema does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// The configuration.
    pub config: Config,
    /// Dotted paths of unrecognised keys, to be shown as warnings.
    pub unknown_keys: Vec<String>,
}

impl Config {
    /// Parses config text, rejecting plaintext secrets and collecting unknown keys.
    ///
    /// # Errors
    /// Returns [`ConfigError::Syntax`] if the text is not valid for the schema, or
    /// [`ConfigError::PlaintextSecret`] if a provider section holds a literal secret.
    pub fn from_toml_str(text: &str) -> Result<Parsed, ConfigError> {
        let value: toml::Value = toml::from_str(text)?;
        if let Some(providers) = value.get(PROVIDERS_KEY) {
            find_plaintext_secret(providers, PROVIDERS_KEY)?;
        }
        let mut unknown_keys = Vec::new();
        let config = serde_ignored::deserialize(value, |path| unknown_keys.push(path.to_string()))?;
        Ok(Parsed {
            config,
            unknown_keys,
        })
    }

    /// Reads and parses the file at `path`.
    ///
    /// # Errors
    /// Returns [`ConfigError::Io`] if the file cannot be read,
    /// [`ConfigError::Parse`] if it is not valid, or
    /// [`ConfigError::PlaintextSecret`] if a provider section holds a literal secret.
    pub fn load(path: &Path) -> Result<Parsed, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&text).map_err(|err| match err {
            ConfigError::Syntax(source) => ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            },
            other => other,
        })
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_lowercase();
    if key.ends_with(ENV_NAME_SUFFIX) {
        return false;
    }
    let key = key.strip_suffix('s').unwrap_or(&key);
    SECRET_KEY_SUFFIXES.iter().any(|s| key.ends_with(s))
}

fn holds_string(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(_) => true,
        toml::Value::Array(items) => items.iter().any(holds_string),
        _ => false,
    }
}

fn find_plaintext_secret(value: &toml::Value, path: &str) -> Result<(), ConfigError> {
    let toml::Value::Table(table) = value else {
        return Ok(());
    };
    for (key, child) in table {
        let child_path = format!("{path}.{key}");
        if is_secret_key(key) && holds_string(child) {
            return Err(ConfigError::PlaintextSecret { path: child_path });
        }
        find_plaintext_secret(child, &child_path)?;
    }
    Ok(())
}

/// Where the config file and data directory live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    base: PathBuf,
}

impl Dirs {
    /// Uses `base` as the root holding `config.toml` and `data/`.
    pub fn from_base(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    /// Resolves the base from the process environment (`CODA_HOME`, else `HOME`).
    ///
    /// # Errors
    /// Returns [`ConfigError::NoHomeDir`] if neither variable is set.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    /// Resolves the base with `lookup` standing in for the environment.
    ///
    /// # Errors
    /// Returns [`ConfigError::NoHomeDir`] if `lookup` finds neither variable.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self, ConfigError> {
        if let Some(home) = lookup(CODA_HOME_VAR) {
            return Ok(Self::from_base(home));
        }
        let home = lookup(HOME_VAR).ok_or(ConfigError::NoHomeDir)?;
        Ok(Self::from_base(
            PathBuf::from(home).join(DEFAULT_BASE_UNDER_HOME),
        ))
    }

    /// The root holding `config.toml`, `data/` and `run/`.
    #[must_use]
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Path of `config.toml`.
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.base.join(CONFIG_FILE_NAME)
    }

    /// Path of the data directory.
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        self.base.join(DATA_DIR_NAME)
    }

    /// Directory holding the daemon's socket, lock and backend pidfiles.
    #[must_use]
    pub fn run_dir(&self) -> PathBuf {
        self.base.join(RUN_DIR_NAME)
    }

    /// Path of the daemon's Unix socket, in a directory only this user can enter.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        self.run_dir().join(SOCKET_FILE_NAME)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::*;

    fn parse(s: &str) -> Parsed {
        Config::from_toml_str(s).expect("valid config")
    }

    #[test]
    fn empty_file_yields_defaults() {
        // Arrange
        let input = "";

        // Act
        let parsed = parse(input);

        // Assert
        assert_eq!(parsed.config.permissions.mode, PermissionMode::Prompt);
        assert_eq!(parsed.config.permissions.grants, Vec::<String>::new());
        assert_eq!(parsed.config.max_turns, 50);
        assert_eq!(parsed.config.token_cap, 1_000_000);
        assert!(parsed.config.providers.is_empty());
        assert_eq!(parsed.config.telemetry.otlp_endpoint, None);
        assert!(!parsed.config.telemetry.capture_content);
        assert_eq!(parsed.config.timing, crate::timing::Timing::default());
        assert_eq!(parsed.unknown_keys, Vec::<String>::new());
    }

    #[test]
    fn full_sample_file_is_parsed() {
        // Arrange
        let input = r#"
max_turns = 7
token_cap = 4200

[permissions]
mode = "accept-edits"
grants = ["read:*", "bash:ls"]

[providers.claude]
enabled = true
api_key_env = "ANTHROPIC_API_KEY"
env_passthrough = ["HTTPS_PROXY"]
verified_versions = ["2.1.0", "2.2.0"]
binary = "/opt/claude/bin/claude"

[telemetry]
otlp_endpoint = "http://localhost:4318"
capture_content = true

[timing]
idle_timeout_ms = 1500
"#;

        // Act
        let parsed = parse(input);

        // Assert
        let c = parsed.config;
        assert_eq!(c.max_turns, 7);
        assert_eq!(c.token_cap, 4200);
        assert_eq!(c.permissions.mode, PermissionMode::AcceptEdits);
        assert_eq!(c.permissions.grants, vec!["read:*", "bash:ls"]);
        let p = &c.providers["claude"];
        assert!(p.enabled);
        assert_eq!(p.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(p.env_passthrough, vec!["HTTPS_PROXY"]);
        assert_eq!(p.verified_versions, vec!["2.1.0", "2.2.0"]);
        assert_eq!(
            p.binary.as_deref(),
            Some(Path::new("/opt/claude/bin/claude"))
        );
        assert_eq!(p.script, None);
        assert_eq!(
            c.telemetry.otlp_endpoint.as_deref(),
            Some("http://localhost:4318")
        );
        assert!(c.telemetry.capture_content);
        assert_eq!(
            c.timing.idle_timeout,
            std::time::Duration::from_millis(1500)
        );
        assert_eq!(c.timing.tool_execution, std::time::Duration::from_secs(300));
        assert_eq!(parsed.unknown_keys, Vec::<String>::new());
    }

    #[test]
    fn each_permission_mode_is_parsed() {
        for (text, expected) in [
            ("prompt", PermissionMode::Prompt),
            ("deny-all", PermissionMode::DenyAll),
            ("accept-edits", PermissionMode::AcceptEdits),
        ] {
            // Arrange
            let input = format!("[permissions]\nmode = \"{text}\"\n");

            // Act
            let parsed = parse(&input);

            // Assert
            assert_eq!(parsed.config.permissions.mode, expected, "mode {text}");
        }
    }

    #[test]
    fn plaintext_secret_keys_are_rejected() {
        for key in [
            "api_key",
            "openai_api_key",
            "API_KEY",
            "apikey",
            "token",
            "access_token",
            "auth_token",
            "secret",
            "client_secret",
            "password",
            "passwd",
            "credential",
            "credentials",
        ] {
            // Arrange
            let input = format!("[providers.claude]\n{key} = \"sk-live-123\"\n");

            // Act
            let err = Config::from_toml_str(&input).expect_err("must reject");

            // Assert
            assert!(
                matches!(&err, ConfigError::PlaintextSecret { path } if path == &format!("providers.claude.{key}")),
                "key {key}: got {err:?}"
            );
        }
    }

    #[test]
    fn plaintext_secret_in_array_is_rejected() {
        // Arrange
        let input = "[providers.claude]\napi_key = [\"sk-live-123\"]\n";

        // Act
        let err = Config::from_toml_str(input).expect_err("must reject");

        // Assert
        assert!(
            matches!(&err, ConfigError::PlaintextSecret { path } if path == "providers.claude.api_key"),
            "got {err:?}"
        );
    }

    #[test]
    fn api_key_env_name_is_accepted() {
        // Arrange
        let input = "[providers.claude]\napi_key_env = \"MY_KEY\"\n";

        // Act
        let parsed = parse(input);

        // Assert
        assert_eq!(
            parsed.config.providers["claude"].api_key_env.as_deref(),
            Some("MY_KEY")
        );
    }

    #[test]
    fn unknown_keys_are_reported_with_dotted_paths() {
        // Arrange
        let input = "bogus = 1\n[permissions]\nmode = \"prompt\"\nextra = true\n[providers.claude]\nflavor = \"x\"\n";

        // Act
        let mut keys = parse(input).unknown_keys;

        // Assert
        keys.sort();
        assert_eq!(
            keys,
            vec!["bogus", "permissions.extra", "providers.claude.flavor"]
        );
    }

    #[test]
    fn faux_script_path_is_parsed() {
        // Arrange
        let input = "[providers.faux]\nenabled = true\nscript = \"/tmp/script.json\"\n";

        // Act
        let parsed = parse(input);

        // Assert
        assert_eq!(
            parsed.config.providers["faux"].script,
            Some(PathBuf::from("/tmp/script.json"))
        );
    }

    #[test]
    fn invalid_toml_error_carries_path() {
        // Arrange
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "max_turns = [").expect("write");

        // Act
        let err = Config::load(&path).expect_err("must fail");

        // Assert
        assert!(matches!(&err, ConfigError::Parse { path: p, .. } if p == &path));
        assert!(err.to_string().contains("config.toml"));
    }

    #[test]
    fn missing_file_error_carries_path() {
        // Arrange
        let path = PathBuf::from("/nonexistent/coda/config.toml");

        // Act
        let err = Config::load(&path).expect_err("must fail");

        // Assert
        assert!(matches!(&err, ConfigError::Io { path: p, .. } if p == &path));
    }

    #[test]
    fn coda_home_overrides_base_dirs() {
        // Arrange
        let lookup = |name: &str| match name {
            "CODA_HOME" => Some(OsString::from("/srv/coda")),
            "HOME" => Some(OsString::from("/home/u")),
            _ => None,
        };

        // Act
        let dirs = Dirs::from_lookup(lookup).expect("dirs");

        // Assert
        assert_eq!(dirs.config_file(), PathBuf::from("/srv/coda/config.toml"));
        assert_eq!(dirs.data_dir(), PathBuf::from("/srv/coda/data"));
    }

    #[test]
    fn home_is_used_when_coda_home_unset() {
        // Arrange
        let lookup = |name: &str| (name == "HOME").then(|| OsString::from("/home/u"));

        // Act
        let dirs = Dirs::from_lookup(lookup).expect("dirs");

        // Assert
        assert_eq!(
            dirs.config_file(),
            PathBuf::from("/home/u/.config/coda/config.toml")
        );
        assert_eq!(dirs.data_dir(), PathBuf::from("/home/u/.config/coda/data"));
    }

    #[test]
    fn no_home_and_no_coda_home_is_an_error() {
        // Arrange
        let lookup = |_: &str| None;

        // Act
        let err = Dirs::from_lookup(lookup).expect_err("must fail");

        // Assert
        assert!(matches!(err, ConfigError::NoHomeDir));
    }

    #[test]
    fn injected_base_builds_paths() {
        // Arrange / Act
        let dirs = Dirs::from_base("/x/y");

        // Assert
        assert_eq!(dirs.config_file(), PathBuf::from("/x/y/config.toml"));
        assert_eq!(dirs.data_dir(), PathBuf::from("/x/y/data"));
        assert_eq!(dirs.socket_path(), PathBuf::from("/x/y/run/coda.sock"));
        assert_eq!(dirs.base(), Path::new("/x/y"));
    }
}
