// SPDX-License-Identifier: AGPL-3.0-only

//! The `config.toml` model, its parser, and the config/data directory layout.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::hooks::{HookConfig, MAX_TIMEOUT_MS};
use crate::routing::{Routing, Tier};
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
const MODELS_KEY: &str = "models";
const ROUTING_KEY: &str = "routing";
const HOOKS_KEY: &str = "hooks";
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
/// The values `sandbox` accepts, the names Codex gives its sandbox modes.
pub const SANDBOX_MODES: [&str; 3] = ["read-only", "workspace-write", "danger-full-access"];

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
    /// A key holds a value outside the ones it accepts.
    #[error("invalid value at `{path}`: use one of {allowed}")]
    InvalidValue {
        /// Dotted path of the offending key.
        path: String,
        /// The accepted values.
        allowed: String,
    },
    /// Two aliases for one provider and model give it different context windows.
    #[error(
        "`{second}` and `{first}` name the same model but set different context windows; \
         keep one value"
    )]
    ConflictingContextWindow {
        /// Dotted path of the alias that was read first.
        first: String,
        /// Dotted path of the alias that disagrees with it.
        second: String,
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
    /// File the `faux` backend appends one JSON line per model request to.
    pub request_log: Option<PathBuf>,
    /// API root of a model provider; the provider's own default when unset.
    pub base_url: Option<String>,
    /// Model a session uses when it names none.
    pub model: Option<String>,
    /// Sandbox a Codex session runs under; read-only when unset.
    pub sandbox: Option<String>,
    /// Allows a `sandbox` wider than read-only.
    pub allow_wider_sandbox: bool,
}

/// One `[models.<alias>]` section: a name that selects a provider and model.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ModelAlias {
    /// Key of the `[providers.<name>]` section that serves the model.
    pub provider: String,
    /// Model id sent to the provider.
    pub model: String,
    /// Context window of the model in tokens, when the built-in table has no
    /// value for it or is wrong.
    #[serde(default)]
    pub context_window: Option<u32>,
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
    /// Model aliases by name.
    pub models: BTreeMap<String, ModelAlias>,
    /// Telemetry settings.
    pub telemetry: TelemetryConfig,
    /// Timeout overrides.
    pub timing: Timing,
    /// Model alias per agent tier.
    pub routing: Routing,
    /// Hooks run at session start and around tool calls; only this file can add them.
    pub hooks: Vec<HookConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            permissions: Permissions::default(),
            max_turns: DEFAULT_MAX_TURNS,
            token_cap: DEFAULT_TOKEN_CAP,
            providers: BTreeMap::new(),
            models: BTreeMap::new(),
            telemetry: TelemetryConfig::default(),
            timing: Timing::default(),
            routing: Routing::default(),
            hooks: Vec::new(),
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
    /// [`ConfigError::PlaintextSecret`] if a provider section holds a literal secret, or
    /// [`ConfigError::InvalidValue`] if a provider `sandbox` is not a Codex sandbox or a
    /// `context_window` or `timing.delegated_tool_ms` is zero, or [`ConfigError::ConflictingContextWindow`] if two
    /// aliases of one model disagree on it.
    pub fn from_toml_str(text: &str) -> Result<Parsed, ConfigError> {
        let value: toml::Value = toml::from_str(text)?;
        if let Some(providers) = value.get(PROVIDERS_KEY) {
            find_plaintext_secret(providers, PROVIDERS_KEY)?;
        }
        let mut unknown_keys = Vec::new();
        let config: Self =
            serde_ignored::deserialize(value, |path| unknown_keys.push(path.to_string()))?;
        config.check_sandboxes()?;
        config.check_context_windows()?;
        config.check_timing()?;
        config.check_routing()?;
        config.check_hooks()?;
        Ok(Parsed {
            config,
            unknown_keys,
        })
    }

    /// The `[models]` alias an agent tier runs on, or `None` when the tier has
    /// no entry and the default model applies.
    #[must_use]
    pub fn model_alias_for(&self, tier: Tier) -> Option<&str> {
        self.routing.0.get(&tier).map(String::as_str)
    }

    fn check_routing(&self) -> Result<(), ConfigError> {
        for (tier, alias) in &self.routing.0 {
            if !self.models.contains_key(alias) {
                return Err(ConfigError::InvalidValue {
                    path: format!("{ROUTING_KEY}.{tier}"),
                    allowed: format!("the name of a [{MODELS_KEY}] alias, not `{alias}`"),
                });
            }
        }
        Ok(())
    }

    fn check_hooks(&self) -> Result<(), ConfigError> {
        for (index, hook) in self.hooks.iter().enumerate() {
            let Some(program) = hook.command.first().filter(|program| !program.is_empty()) else {
                return Err(ConfigError::InvalidValue {
                    path: format!("{HOOKS_KEY}[{index}].command"),
                    allowed: "a list with the program first, then its arguments".to_owned(),
                });
            };
            // A relative path would run a file of the session directory, which the model can edit.
            if program.contains(std::path::is_separator) && !Path::new(program).is_absolute() {
                return Err(ConfigError::InvalidValue {
                    path: format!("{HOOKS_KEY}[{index}].command"),
                    allowed: format!(
                        "a program that is an absolute path or a bare name found on PATH, not `{program}`"
                    ),
                });
            }
            if hook
                .timeout_ms
                .is_some_and(|ms| ms == 0 || ms > MAX_TIMEOUT_MS)
            {
                return Err(ConfigError::InvalidValue {
                    path: format!("{HOOKS_KEY}[{index}].timeout_ms"),
                    allowed: format!("a whole number of milliseconds from 1 to {MAX_TIMEOUT_MS}"),
                });
            }
        }
        Ok(())
    }

    fn check_sandboxes(&self) -> Result<(), ConfigError> {
        for (name, section) in &self.providers {
            if let Some(sandbox) = &section.sandbox
                && !SANDBOX_MODES.contains(&sandbox.as_str())
            {
                return Err(ConfigError::InvalidValue {
                    path: format!("{PROVIDERS_KEY}.{name}.sandbox"),
                    allowed: SANDBOX_MODES.join(", "),
                });
            }
        }
        Ok(())
    }

    fn check_timing(&self) -> Result<(), ConfigError> {
        if self.timing.delegated_tool.is_zero() {
            return Err(ConfigError::InvalidValue {
                path: "timing.delegated_tool_ms".to_owned(),
                allowed: "a whole number of milliseconds above 0".to_owned(),
            });
        }
        Ok(())
    }

    fn check_context_windows(&self) -> Result<(), ConfigError> {
        let mut seen: BTreeMap<(&str, &str), (&str, u32)> = BTreeMap::new();
        for (name, alias) in &self.models {
            let Some(window) = alias.context_window else {
                continue;
            };
            if window == 0 {
                return Err(ConfigError::InvalidValue {
                    path: format!("{MODELS_KEY}.{name}.context_window"),
                    allowed: "a whole number of tokens above 0".to_owned(),
                });
            }
            let key = (alias.provider.as_str(), alias.model.as_str());
            match seen.get(&key) {
                Some(&(first, other)) if other != window => {
                    return Err(ConfigError::ConflictingContextWindow {
                        first: format!("{MODELS_KEY}.{first}.context_window"),
                        second: format!("{MODELS_KEY}.{name}.context_window"),
                    });
                }
                _ => {
                    seen.insert(key, (name, window));
                }
            }
        }
        Ok(())
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
    fn sandbox_settings_default_to_read_only_without_an_opt_in() {
        // Arrange
        let wide = "[providers.codex]\nsandbox = \"workspace-write\"\nallow_wider_sandbox = true\n";

        // Act
        let (plain, opted_in) = (parse("[providers.codex]\nenabled = true\n"), parse(wide));

        // Assert
        let (plain, opted_in) = (
            &plain.config.providers["codex"],
            &opted_in.config.providers["codex"],
        );
        assert_eq!(
            (plain.sandbox.as_deref(), plain.allow_wider_sandbox),
            (None, false)
        );
        assert_eq!(
            (opted_in.sandbox.as_deref(), opted_in.allow_wider_sandbox),
            (Some("workspace-write"), true)
        );
    }

    #[test]
    fn an_unknown_sandbox_is_rejected_at_load_naming_the_key() {
        // Arrange
        let text = "[providers.codex]\nsandbox = \"workspace-writ\"\n";

        // Act
        let err = Config::from_toml_str(text).unwrap_err();

        // Assert
        assert!(
            matches!(&err, ConfigError::InvalidValue { path, .. } if path == "providers.codex.sandbox"),
            "{err}"
        );
        assert!(err.to_string().contains("danger-full-access"));
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
        assert_eq!(c.timing.delegated_tool, std::time::Duration::from_mins(15));
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
    fn model_aliases_and_provider_defaults_are_parsed() {
        // Arrange
        let input = "[providers.openai_compat]\nbase_url = \"http://localhost:1/v1\"\nmodel = \"m\"\n\
                     [models.local]\nprovider = \"openai_compat\"\nmodel = \"qwen\"\n";

        // Act
        let parsed = parse(input);

        // Assert
        let provider = &parsed.config.providers["openai_compat"];
        assert_eq!(provider.base_url.as_deref(), Some("http://localhost:1/v1"));
        assert_eq!(provider.model.as_deref(), Some("m"));
        assert_eq!(parsed.config.models["local"].model, "qwen");
        assert_eq!(parsed.unknown_keys, Vec::<String>::new());
    }

    #[test]
    fn a_model_alias_may_set_its_context_window() {
        // Arrange
        let input = "[models.big]\nprovider = \"anthropic\"\nmodel = \"m\"\ncontext_window = 1000000\n\
                     [models.plain]\nprovider = \"anthropic\"\nmodel = \"n\"\n";

        // Act
        let parsed = parse(input);

        // Assert
        assert_eq!(parsed.config.models["big"].context_window, Some(1_000_000));
        assert_eq!(parsed.config.models["plain"].context_window, None);
        assert_eq!(parsed.unknown_keys, Vec::<String>::new());
    }

    #[test]
    fn a_zero_context_window_is_rejected_naming_the_key() {
        // Arrange
        let input = "[models.big]\nprovider = \"anthropic\"\nmodel = \"m\"\ncontext_window = 0\n";

        // Act
        let err = Config::from_toml_str(input).unwrap_err();

        // Assert
        assert!(
            matches!(&err, ConfigError::InvalidValue { path, .. } if path == "models.big.context_window"),
            "{err}"
        );
    }

    #[test]
    fn a_zero_delegated_tool_bound_is_rejected_naming_the_key() {
        // Arrange
        let input = "[timing]\ndelegated_tool_ms = 0\n";

        // Act
        let err = Config::from_toml_str(input).unwrap_err();

        // Assert
        assert!(
            matches!(&err, ConfigError::InvalidValue { path, .. } if path == "timing.delegated_tool_ms"),
            "{err}"
        );
    }

    #[test]
    fn aliases_of_one_model_must_agree_on_its_context_window() {
        // Arrange
        let alias = |name: &str, window: u32| {
            format!(
                "[models.{name}]\nprovider = \"anthropic\"\nmodel = \"m\"\ncontext_window = {window}\n"
            )
        };
        let agree = format!("{}{}", alias("a", 5000), alias("b", 5000));
        let clash = format!("{}{}", alias("a", 5000), alias("b", 9000));

        // Act
        let agreed = Config::from_toml_str(&agree);
        let clashed = Config::from_toml_str(&clash).unwrap_err();

        // Assert
        assert!(agreed.is_ok());
        assert!(
            matches!(&clashed, ConfigError::ConflictingContextWindow { first, second }
                if first == "models.a.context_window" && second == "models.b.context_window"),
            "{clashed}"
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
