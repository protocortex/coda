// SPDX-License-Identifier: Apache-2.0

//! The `config.toml` model, its parser, and the config/data directory layout.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::checkpoints::{
    CHECKPOINTS_KEY, CheckpointsConfig, KEEP_DAYS_KEY, KEEP_TURNS_KEY, PRUNE_KEY, REWIND_DAYS_KEY,
};
use crate::checks::CheckConfig;
use crate::daemon::DaemonConfig;
use crate::describe::{
    DESCRIBE_KEY, DescribeConfig, LABEL_NAME_KEY, LABEL_USE_WHEN_KEY, LABELS_KEY,
};
use crate::fetch::{
    ALLOW_HOSTS_KEY, FETCH_KEY, FETCH_TIMEOUT_LIMIT_MS, FetchConfig, MAX_BYTES_KEY,
    MAX_BYTES_LIMIT, TIMEOUT_MS_KEY, host_problem,
};
use crate::hooks::{HookConfig, MAX_TIMEOUT_MS};
use crate::loop_guard::{
    LOOP_GUARD_KEY, LoopGuardConfig, MIN_WARN_AFTER, STOP_AFTER_KEY, WARN_AFTER_KEY,
};
use crate::recall::{DELEGATED_REPOS_KEY, RECALL_KEY, RecallConfig, is_repo_path};
use crate::repo_map::{MAX_TOKEN_SHARE_PERCENT, REPO_MAP_KEY, RepoMapConfig, TOKEN_SHARE_KEY};
use crate::review::{
    CHECK_COMMANDS_KEY, CHUNK_CONCURRENCY_KEY, DIFF_BUDGET_CHARS_KEY, MAX_CHUNKS_KEY,
    MAX_FINDINGS_KEY, REVIEW_KEY, ROUTING_MODEL_KEY, ROUTING_RULES_PATH, ReviewConfig,
    VERIFY_THRESHOLD_KEY, VERIFY_THRESHOLD_MAX,
};
use crate::routing::{Routing, Tier};
use crate::timing::Timing;
use crate::tool_servers::{
    COMMAND_KEY, ENV_KEY, MAX_ENV_NAMES, MAX_SERVER_TIMEOUT_MS, MAX_SERVERS, SERVER_TIMEOUT_MS_KEY,
    TOOL_SERVERS_KEY, ToolServerConfig, env_name_problem, name_problem,
};
use crate::tui::TuiConfig;
use crate::worktree::{
    CLEAN_COMMAND_KEY, CLEANUP_KEY, DIR_KEY, IDLE_DAYS_KEY, SETUP_KEY, WORKTREE_KEY, WorktreeConfig,
};

/// Output limit of a model request when `max_output_tokens` is unset.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;
/// The least thinking budget the Anthropic API accepts.
pub const MIN_THINKING_BUDGET_TOKENS: u32 = 1024;
const ANTHROPIC_KEY: &str = "anthropic";
const DEFAULT_MAX_TURNS: u32 = 50;
const DEFAULT_TOKEN_CAP: u64 = 1_000_000;
/// Times an `end_turn` hook may send the model back to work in one send.
pub const DEFAULT_MAX_GUARD_CONTINUES: u32 = 2;
/// Read-only tool calls from one reply that may run at the same time.
pub const DEFAULT_MAX_PARALLEL_TOOLS: u32 = 4;
const MAX_PARALLEL_TOOLS_KEY: &str = "max_parallel_tools";

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
const FALLBACK_KEY: &str = "fallback";
/// The provider sections that call a model API themselves. The others hand the
/// whole turn to another program, so there is no model call to fall back from.
const INDEPENDENT_PROVIDERS: [&str; 3] = ["anthropic", "openai_compat", "faux"];
const HOOKS_KEY: &str = "hooks";
const CHECKS_KEY: &str = "checks";
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

/// The request field that caps the reply length on an `openai_compat` server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    /// `max_tokens`, which most servers accept.
    #[default]
    MaxTokens,
    /// `max_completion_tokens`, which reasoning models such as the o-series require.
    MaxCompletionTokens,
}

impl MaxTokensField {
    /// The key as it appears in `config.toml` and in the request body.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

/// How much of Claude Code's own setup a `claude_code` session keeps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeProfile {
    /// Claude Code as it starts alone: your user and project settings, MCP servers,
    /// skills and its full prompt.
    Full,
    /// Project settings only, no extra MCP servers and no skills or slash commands.
    #[default]
    Project,
    /// Coda's own short prompt, a short tool list and no settings at all.
    Lean,
}

impl ClaudeProfile {
    /// The value as it appears in `config.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Project => "project",
            Self::Lean => "lean",
        }
    }
}

/// One `[providers.<backend>]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is its own key in config.toml"
)]
pub struct ProviderConfig {
    /// Whether the backend may be used.
    pub enabled: bool,
    /// Explicit path of the backend binary; searched on the path when unset.
    pub binary: Option<PathBuf>,
    /// Name of the environment variable holding the API key.
    pub api_key_env: Option<String>,
    /// Extra environment variables forwarded to the backend process.
    pub env_passthrough: Vec<String>,
    /// What a Claude Code session keeps of its own setup; `claude_code` only.
    pub profile: ClaudeProfile,
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
    /// Lets a Codex app-server keep its thread in Codex's own saved history, so
    /// a restarted daemon resumes the thread instead of replaying a text seed.
    /// Off by default, which keeps the thread out of Codex's history; `codex` only.
    pub persist_threads: bool,
    /// Output token limit of a model request; the built-in limit when unset.
    pub max_output_tokens: Option<u32>,
    /// Tokens the model may spend thinking before it answers; the request asks
    /// for no thinking when unset. `anthropic` only.
    pub thinking_budget_tokens: Option<u32>,
    /// Request field that carries the output limit; `openai_compat` only.
    pub max_tokens_field: MaxTokensField,
    /// Whether the request asks for token usage in the stream; `openai_compat` only.
    pub stream_usage: bool,
    /// Whether a reply that is one JSON object fitting exactly one tool counts as
    /// a call to it; for a model that writes its calls as text. Model-API providers only.
    pub text_tool_calls: bool,
}

// Written by hand because `stream_usage` is on unless the file turns it off.
impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            binary: None,
            api_key_env: None,
            env_passthrough: Vec::new(),
            profile: ClaudeProfile::default(),
            verified_versions: Vec::new(),
            script: None,
            request_log: None,
            base_url: None,
            model: None,
            sandbox: None,
            allow_wider_sandbox: false,
            persist_threads: false,
            max_output_tokens: None,
            thinking_budget_tokens: None,
            max_tokens_field: MaxTokensField::default(),
            stream_usage: true,
            text_tool_calls: false,
        }
    }
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
    /// Aliases to try, in order, when this model fails with a rate limit that
    /// outlasts the retries, an overload or an unknown-model error. Every alias
    /// in the chain must be on a model-API provider.
    #[serde(default)]
    pub fallback: Vec<String>,
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
    /// Most times an `end_turn` hook may send the model back to work in one send.
    pub max_guard_continues: u32,
    /// Most read-only tool calls from one model reply that run at the same time.
    pub max_parallel_tools: u32,
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
    /// Commands run after a file change, by file extension; only this file can add them.
    pub checks: BTreeMap<String, CheckConfig>,
    /// Limits for `coda review`.
    pub review: ReviewConfig,
    /// Thresholds for the stall and loop detector.
    pub loop_guard: LoopGuardConfig,
    /// Settings of the terminal UI.
    pub tui: TuiConfig,
    /// How the daemon treats work a restart cut short.
    pub daemon: DaemonConfig,
    /// Where session worktrees go and what runs in a new one.
    pub worktree: WorktreeConfig,
    /// When old checkpoint refs are removed.
    pub checkpoints: CheckpointsConfig,
    /// Labels `coda describe --labels` may suggest.
    pub describe: DescribeConfig,
    /// The hosts the `fetch` tool may read; off while the list is empty.
    pub fetch: FetchConfig,
    /// Whether earlier sessions are indexed for the `recall` tool; off by default.
    pub recall: RecallConfig,
    /// The outline of the repository a model-API session starts with.
    pub repo_map: RepoMapConfig,
    /// External tool servers by name; none until the user lists one.
    pub tool_servers: BTreeMap<String, ToolServerConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            permissions: Permissions::default(),
            max_turns: DEFAULT_MAX_TURNS,
            token_cap: DEFAULT_TOKEN_CAP,
            max_guard_continues: DEFAULT_MAX_GUARD_CONTINUES,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            providers: BTreeMap::new(),
            models: BTreeMap::new(),
            telemetry: TelemetryConfig::default(),
            timing: Timing::default(),
            routing: Routing::default(),
            hooks: Vec::new(),
            checks: BTreeMap::new(),
            review: ReviewConfig::default(),
            loop_guard: LoopGuardConfig::default(),
            tui: TuiConfig::default(),
            daemon: DaemonConfig::default(),
            worktree: WorktreeConfig::default(),
            checkpoints: CheckpointsConfig::default(),
            describe: DescribeConfig::default(),
            fetch: FetchConfig::default(),
            recall: RecallConfig::default(),
            repo_map: RepoMapConfig::default(),
            tool_servers: BTreeMap::new(),
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
    /// `context_window`, `max_output_tokens` or `timing.delegated_tool_ms` is zero, or [`ConfigError::ConflictingContextWindow`] if two
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
        config.check_env_passthrough()?;
        config.check_output_limits()?;
        config.check_context_windows()?;
        config.check_timing()?;
        config.check_routing()?;
        config.check_fallbacks()?;
        config.check_hooks()?;
        config.check_checks()?;
        config.check_review()?;
        config.check_describe()?;
        config.check_loop_guard()?;
        config.check_parallel_tools()?;
        config.check_worktree()?;
        config.check_checkpoints()?;
        config.check_fetch()?;
        config.check_recall()?;
        config.check_repo_map()?;
        config.check_tool_servers()?;
        Ok(Parsed {
            config,
            unknown_keys,
        })
    }

    /// The `[models]` alias an agent tier runs on, or `None` when the tier has
    /// no entry and the model of the session that starts the agent applies.
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

    fn check_fallbacks(&self) -> Result<(), ConfigError> {
        for (name, alias) in &self.models {
            if alias.fallback.is_empty() {
                continue;
            }
            let path = format!("{MODELS_KEY}.{name}.{FALLBACK_KEY}");
            let invalid = |allowed: String| ConfigError::InvalidValue {
                path: path.clone(),
                allowed,
            };
            if !INDEPENDENT_PROVIDERS.contains(&alias.provider.as_str()) {
                return Err(invalid(format!(
                    "a list only on a model-API provider ({}), not `{}`",
                    INDEPENDENT_PROVIDERS.join(", "),
                    alias.provider
                )));
            }
            let mut seen = std::collections::BTreeSet::from([name.as_str()]);
            for target in &alias.fallback {
                let Some(next) = self.models.get(target) else {
                    return Err(invalid(format!(
                        "the names of [{MODELS_KEY}] aliases, not `{target}`"
                    )));
                };
                if !INDEPENDENT_PROVIDERS.contains(&next.provider.as_str()) {
                    return Err(invalid(format!(
                        "aliases on a model-API provider ({}), but `{target}` uses `{}`",
                        INDEPENDENT_PROVIDERS.join(", "),
                        next.provider
                    )));
                }
                if !seen.insert(target.as_str()) {
                    return Err(invalid(format!(
                        "each alias once and never `{name}` itself, but `{target}` repeats"
                    )));
                }
            }
        }
        Ok(())
    }

    fn check_checks(&self) -> Result<(), ConfigError> {
        let mut seen = std::collections::BTreeSet::new();
        for (extension, check) in &self.checks {
            let name_is_plain = !extension.is_empty()
                && extension
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'));
            if !name_is_plain {
                return Err(ConfigError::InvalidValue {
                    path: format!("{CHECKS_KEY}.{extension}"),
                    allowed: "an extension without a dot, such as `rs`".to_owned(),
                });
            }
            // Extensions are matched without regard to case, so `RS` would shadow `rs`.
            if !seen.insert(extension.to_ascii_lowercase()) {
                return Err(ConfigError::InvalidValue {
                    path: format!("{CHECKS_KEY}.{extension}"),
                    allowed: "one entry per extension, whatever its case".to_owned(),
                });
            }
            check_command(&format!("{CHECKS_KEY}.{extension}.command"), &check.command)?;
            if check
                .timeout_ms
                .is_some_and(|ms| ms == 0 || ms > MAX_TIMEOUT_MS)
            {
                return Err(ConfigError::InvalidValue {
                    path: format!("{CHECKS_KEY}.{extension}.timeout_ms"),
                    allowed: format!("a whole number of milliseconds from 1 to {MAX_TIMEOUT_MS}"),
                });
            }
        }
        Ok(())
    }

    fn check_hooks(&self) -> Result<(), ConfigError> {
        for (index, hook) in self.hooks.iter().enumerate() {
            check_command(&format!("{HOOKS_KEY}[{index}].command"), &hook.command)?;
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

    fn check_env_passthrough(&self) -> Result<(), ConfigError> {
        for (name, section) in &self.providers {
            if !section.env_passthrough.iter().all(|var| is_env_name(var)) {
                return Err(ConfigError::InvalidValue {
                    path: format!("{PROVIDERS_KEY}.{name}.env_passthrough"),
                    allowed: "variable names made of letters, digits and underscores, with no `=` and no value"
                        .to_owned(),
                });
            }
        }
        Ok(())
    }

    fn check_output_limits(&self) -> Result<(), ConfigError> {
        for (name, section) in &self.providers {
            if section.max_output_tokens == Some(0) {
                return Err(ConfigError::InvalidValue {
                    path: format!("{PROVIDERS_KEY}.{name}.max_output_tokens"),
                    allowed: "a whole number of tokens above 0".to_owned(),
                });
            }
            if let Some(budget) = section
                .thinking_budget_tokens
                .filter(|_| name == ANTHROPIC_KEY)
            {
                let limit = section
                    .max_output_tokens
                    .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
                if !(MIN_THINKING_BUDGET_TOKENS..limit).contains(&budget) {
                    return Err(ConfigError::InvalidValue {
                        path: format!("{PROVIDERS_KEY}.{name}.thinking_budget_tokens"),
                        allowed: format!(
                            "a whole number of tokens from {MIN_THINKING_BUDGET_TOKENS} up to, not including, the output limit ({limit})"
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    fn check_describe(&self) -> Result<(), ConfigError> {
        let mut seen: Vec<String> = Vec::new();
        for (index, rule) in self.describe.labels.iter().enumerate() {
            let path = |key: &str| format!("{DESCRIBE_KEY}.{LABELS_KEY}.{index}.{key}");
            if rule.name.trim().is_empty() {
                return Err(ConfigError::InvalidValue {
                    path: path(LABEL_NAME_KEY),
                    allowed: "a label name that is not empty".to_owned(),
                });
            }
            if rule.use_when.trim().is_empty() {
                return Err(ConfigError::InvalidValue {
                    path: path(LABEL_USE_WHEN_KEY),
                    allowed: "a line that says when the label fits".to_owned(),
                });
            }
            let name = rule.name.trim().to_lowercase();
            if seen.contains(&name) {
                return Err(ConfigError::InvalidValue {
                    path: path(LABEL_NAME_KEY),
                    allowed: format!("a name not used before, and `{}` is", rule.name),
                });
            }
            seen.push(name);
        }
        Ok(())
    }

    fn check_review(&self) -> Result<(), ConfigError> {
        if self.review.max_findings == 0 {
            return Err(ConfigError::InvalidValue {
                path: format!("{REVIEW_KEY}.{MAX_FINDINGS_KEY}"),
                allowed: "a whole number of findings above 0".to_owned(),
            });
        }
        if self.review.diff_budget_chars == 0 {
            return Err(ConfigError::InvalidValue {
                path: format!("{REVIEW_KEY}.{DIFF_BUDGET_CHARS_KEY}"),
                allowed: "a whole number of characters above 0".to_owned(),
            });
        }
        if self.review.max_chunks == 0 {
            return Err(ConfigError::InvalidValue {
                path: format!("{REVIEW_KEY}.{MAX_CHUNKS_KEY}"),
                allowed: "a whole number of chunks above 0".to_owned(),
            });
        }
        if self.review.verify_threshold > VERIFY_THRESHOLD_MAX {
            return Err(ConfigError::InvalidValue {
                path: format!("{REVIEW_KEY}.{VERIFY_THRESHOLD_KEY}"),
                allowed: format!("a score from 0 to {VERIFY_THRESHOLD_MAX}"),
            });
        }
        if self.review.chunk_concurrency == 0 {
            return Err(ConfigError::InvalidValue {
                path: format!("{REVIEW_KEY}.{CHUNK_CONCURRENCY_KEY}"),
                allowed: "a whole number of chunks above 0".to_owned(),
            });
        }
        for (index, command) in self.review.check_commands.iter().enumerate() {
            if command.trim().is_empty() {
                return Err(ConfigError::InvalidValue {
                    path: format!("{REVIEW_KEY}.{CHECK_COMMANDS_KEY}.{index}"),
                    allowed: "a command line that is not empty".to_owned(),
                });
            }
        }
        for (index, rule) in self.review.routing.rules.iter().enumerate() {
            if rule.model.trim().is_empty() {
                return Err(ConfigError::InvalidValue {
                    path: format!("{REVIEW_KEY}.{ROUTING_RULES_PATH}.{index}.{ROUTING_MODEL_KEY}"),
                    allowed: "a model name that is not empty".to_owned(),
                });
            }
        }
        Ok(())
    }

    fn check_parallel_tools(&self) -> Result<(), ConfigError> {
        if self.max_parallel_tools == 0 {
            return Err(ConfigError::InvalidValue {
                path: MAX_PARALLEL_TOOLS_KEY.to_owned(),
                allowed: "a whole number of tool calls above 0".to_owned(),
            });
        }
        Ok(())
    }

    fn check_loop_guard(&self) -> Result<(), ConfigError> {
        let guard = &self.loop_guard;
        if guard.warn_after < MIN_WARN_AFTER {
            return Err(ConfigError::InvalidValue {
                path: format!("{LOOP_GUARD_KEY}.{WARN_AFTER_KEY}"),
                allowed: format!("a whole number of repeats of at least {MIN_WARN_AFTER}"),
            });
        }
        if guard.stop_after <= guard.warn_after {
            return Err(ConfigError::InvalidValue {
                path: format!("{LOOP_GUARD_KEY}.{STOP_AFTER_KEY}"),
                allowed: format!("a whole number of repeats above {WARN_AFTER_KEY}"),
            });
        }
        Ok(())
    }

    fn check_worktree(&self) -> Result<(), ConfigError> {
        let worktree = &self.worktree;
        if worktree
            .dir
            .as_deref()
            .is_some_and(|dir| !dir.is_absolute())
        {
            return Err(ConfigError::InvalidValue {
                path: format!("{WORKTREE_KEY}.{DIR_KEY}"),
                allowed: "an absolute path".to_owned(),
            });
        }
        if worktree.setup.first().is_some_and(String::is_empty) {
            return Err(ConfigError::InvalidValue {
                path: format!("{WORKTREE_KEY}.{SETUP_KEY}"),
                allowed: "a program name first, then its arguments".to_owned(),
            });
        }
        let cleanup = &worktree.cleanup;
        if cleanup.idle_days == 0 {
            return Err(ConfigError::InvalidValue {
                path: format!("{WORKTREE_KEY}.{CLEANUP_KEY}.{IDLE_DAYS_KEY}"),
                allowed: "a whole number of days above 0".to_owned(),
            });
        }
        if cleanup.clean_command.first().is_some_and(String::is_empty) {
            return Err(ConfigError::InvalidValue {
                path: format!("{WORKTREE_KEY}.{CLEANUP_KEY}.{CLEAN_COMMAND_KEY}"),
                allowed: "a program name first, then its arguments".to_owned(),
            });
        }
        Ok(())
    }

    fn check_fetch(&self) -> Result<(), ConfigError> {
        let fetch = &self.fetch;
        for entry in &fetch.allow_hosts {
            if let Some(allowed) = host_problem(entry) {
                return Err(ConfigError::InvalidValue {
                    path: format!("{FETCH_KEY}.{ALLOW_HOSTS_KEY}"),
                    allowed: format!("{allowed}, not `{entry}`"),
                });
            }
        }
        if fetch.max_bytes == 0 || fetch.max_bytes > MAX_BYTES_LIMIT {
            return Err(ConfigError::InvalidValue {
                path: format!("{FETCH_KEY}.{MAX_BYTES_KEY}"),
                allowed: format!("a whole number of bytes from 1 to {MAX_BYTES_LIMIT}"),
            });
        }
        if fetch.timeout_ms == 0 || fetch.timeout_ms > FETCH_TIMEOUT_LIMIT_MS {
            return Err(ConfigError::InvalidValue {
                path: format!("{FETCH_KEY}.{TIMEOUT_MS_KEY}"),
                allowed: format!(
                    "a whole number of milliseconds from 1 to {FETCH_TIMEOUT_LIMIT_MS}"
                ),
            });
        }
        Ok(())
    }

    fn check_recall(&self) -> Result<(), ConfigError> {
        if self.recall.delegated_repos.iter().all(|r| is_repo_path(r)) {
            return Ok(());
        }
        Err(ConfigError::InvalidValue {
            path: format!("{RECALL_KEY}.{DELEGATED_REPOS_KEY}"),
            allowed: "absolute folder paths".to_owned(),
        })
    }

    fn check_repo_map(&self) -> Result<(), ConfigError> {
        let share = self.repo_map.token_share_percent;
        if (1..=MAX_TOKEN_SHARE_PERCENT).contains(&share) {
            return Ok(());
        }
        Err(ConfigError::InvalidValue {
            path: format!("{REPO_MAP_KEY}.{TOKEN_SHARE_KEY}"),
            allowed: format!("a whole number of percent from 1 to {MAX_TOKEN_SHARE_PERCENT}"),
        })
    }

    fn check_tool_servers(&self) -> Result<(), ConfigError> {
        let invalid = |path: String, allowed: String| ConfigError::InvalidValue { path, allowed };
        if self.tool_servers.len() > MAX_SERVERS {
            return Err(invalid(
                TOOL_SERVERS_KEY.to_owned(),
                format!("at most {MAX_SERVERS} servers"),
            ));
        }
        for (name, server) in &self.tool_servers {
            let at = |key: &str| format!("{TOOL_SERVERS_KEY}.{name}.{key}");
            if let Some(allowed) = name_problem(name) {
                return Err(invalid(
                    format!("{TOOL_SERVERS_KEY}.{name}"),
                    allowed.to_owned(),
                ));
            }
            if let Some(allowed) = server.problem() {
                return Err(invalid(at(COMMAND_KEY), allowed));
            }
            let bad_env = server.env.iter().find_map(|var| env_name_problem(var));
            if server.env.len() > MAX_ENV_NAMES || bad_env.is_some() {
                return Err(invalid(
                    at(ENV_KEY),
                    format!(
                        "at most {MAX_ENV_NAMES} names of {}",
                        bad_env.unwrap_or("environment variables")
                    ),
                ));
            }
            if server.timeout_ms == 0 || server.timeout_ms > MAX_SERVER_TIMEOUT_MS {
                return Err(invalid(
                    at(SERVER_TIMEOUT_MS_KEY),
                    format!("a whole number of milliseconds from 1 to {MAX_SERVER_TIMEOUT_MS}"),
                ));
            }
        }
        Ok(())
    }

    fn check_checkpoints(&self) -> Result<(), ConfigError> {
        let prune = &self.checkpoints.prune;
        for (key, value) in [
            (KEEP_DAYS_KEY, prune.keep_days),
            (KEEP_TURNS_KEY, prune.keep_turns),
            (REWIND_DAYS_KEY, prune.rewind_days),
        ] {
            if value == 0 {
                return Err(ConfigError::InvalidValue {
                    path: format!("{CHECKPOINTS_KEY}.{PRUNE_KEY}.{key}"),
                    allowed: "a whole number above 0".to_owned(),
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

fn is_env_name(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with(|c: char| c.is_ascii_digit())
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
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

/// Refuses a command with no program, or whose program is a relative path.
fn check_command(path: &str, command: &[String]) -> Result<(), ConfigError> {
    let Some(program) = command.first().filter(|program| !program.is_empty()) else {
        return Err(ConfigError::InvalidValue {
            path: path.to_owned(),
            allowed: "a list with the program first, then its arguments".to_owned(),
        });
    };
    // A relative path would run a file of the session directory, which the model can edit.
    if program.contains(std::path::is_separator) && !Path::new(program).is_absolute() {
        return Err(ConfigError::InvalidValue {
            path: path.to_owned(),
            allowed: format!(
                "a program that is an absolute path or a bare name found on PATH, not `{program}`"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::tui::{DEFAULT_CACHE_HINT_TOKENS, DEFAULT_CACHE_TTL_SECS, NotifyMode};

    fn parse(s: &str) -> Parsed {
        Config::from_toml_str(s).expect("valid config")
    }

    #[test]
    fn kept_codex_threads_are_off_until_the_provider_turns_them_on() {
        // Arrange
        let on = "[providers.codex]\nenabled = true\npersist_threads = true\n";

        // Act
        let default = Config::from_toml_str("[providers.codex]\nenabled = true\n").unwrap();
        let set = Config::from_toml_str(on).unwrap();

        // Assert
        assert!(!default.config.providers["codex"].persist_threads);
        assert!(set.config.providers["codex"].persist_threads);
        assert_eq!(set.unknown_keys, Vec::<String>::new());
    }

    #[test]
    fn continuing_after_a_restart_is_off_until_the_daemon_table_turns_it_on() {
        // Arrange
        let on = "[daemon]\ncontinue_after_restart = true\n";

        // Act
        let default = Config::from_toml_str("").unwrap().config.daemon;
        let set = Config::from_toml_str(on).unwrap();

        // Assert
        assert!(!default.continue_after_restart);
        assert!(set.config.daemon.continue_after_restart);
        assert_eq!(set.unknown_keys, Vec::<String>::new());
    }

    #[test]
    fn text_tool_calls_are_off_unless_a_provider_turns_them_on() {
        // Arrange
        let on = "[providers.openai_compat]\nenabled = true\ntext_tool_calls = true\n";
        let off = "[providers.openai_compat]\nenabled = true\n";

        // Act
        let on = parse(on).config;
        let off = parse(off).config;

        // Assert
        assert!(on.providers["openai_compat"].text_tool_calls);
        assert!(!off.providers["openai_compat"].text_tool_calls);
    }

    #[test]
    fn the_parallel_tool_limit_defaults_to_four_and_rejects_zero() {
        // Arrange
        let set = "max_parallel_tools = 2";

        // Act
        let read = parse(set).config;
        let unset = parse("").config;
        let zero = Config::from_toml_str("max_parallel_tools = 0");

        // Assert
        assert_eq!(read.max_parallel_tools, 2);
        assert_eq!(unset.max_parallel_tools, 4);
        assert!(matches!(zero, Err(ConfigError::InvalidValue { .. })));
    }

    #[test]
    fn an_end_turn_hook_and_its_continue_cap_are_read_and_default_to_two() {
        // Arrange
        let input = r#"
max_guard_continues = 5

[[hooks]]
event = "end_turn"
command = ["/bin/true"]
blocking = true
"#;

        // Act
        let set = parse(input).config;
        let unset = parse("").config;

        // Assert
        assert_eq!(set.max_guard_continues, 5);
        assert_eq!(set.hooks[0].event, crate::hooks::HookEvent::EndTurn);
        assert_eq!(unset.max_guard_continues, 2);
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

    fn fallback_config(primary: &str, fallback: &str, other: &str) -> String {
        format!(
            "[models.main]\nprovider = \"{primary}\"\nmodel = \"m\"\nfallback = {fallback}\n\
             [models.other]\nprovider = \"{other}\"\nmodel = \"n\"\n"
        )
    }

    #[test]
    fn a_model_alias_may_list_fallbacks_on_model_api_providers() {
        let input = fallback_config("anthropic", "[\"other\"]", "openai_compat");

        let parsed = parse(&input);

        assert_eq!(parsed.config.models["main"].fallback, vec!["other"]);
        assert_eq!(parsed.config.models["other"].fallback.len(), 0);
        assert_eq!(parsed.unknown_keys, Vec::<String>::new());
    }

    #[test]
    fn a_fallback_chain_with_a_delegated_backend_is_rejected_naming_the_key() {
        for input in [
            fallback_config("anthropic", "[\"other\"]", "claude"),
            fallback_config("codex", "[\"other\"]", "anthropic"),
        ] {
            let err = Config::from_toml_str(&input).unwrap_err();

            assert!(
                matches!(&err, ConfigError::InvalidValue { path, .. } if path == "models.main.fallback"),
                "{err}"
            );
        }
    }

    #[test]
    fn a_fallback_that_is_unknown_repeated_or_the_alias_itself_is_rejected() {
        for list in ["[\"nope\"]", "[\"other\", \"other\"]", "[\"main\"]"] {
            let input = fallback_config("anthropic", list, "anthropic");

            let err = Config::from_toml_str(&input).unwrap_err();

            assert!(
                matches!(&err, ConfigError::InvalidValue { path, .. } if path == "models.main.fallback"),
                "{list}: {err}"
            );
        }
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
    fn review_max_findings_defaults_to_five_and_can_be_set() {
        // Arrange
        let set = "[review]\nmax_findings = 8\n";

        // Act
        let default = Config::from_toml_str("").unwrap().config;
        let custom = Config::from_toml_str(set).unwrap().config;

        // Assert
        assert_eq!(default.review.max_findings, 5);
        assert_eq!(custom.review.max_findings, 8);
    }

    #[test]
    fn review_diff_budget_defaults_to_120000_and_zero_is_rejected() {
        // Arrange
        let set = "[review]\ndiff_budget_chars = 5000\n";
        let zero = "[review]\ndiff_budget_chars = 0\n";

        // Act
        let default = Config::from_toml_str("").unwrap().config;
        let custom = Config::from_toml_str(set).unwrap().config;
        let err = Config::from_toml_str(zero).unwrap_err();

        // Assert
        assert_eq!(default.review.diff_budget_chars, 120_000);
        assert_eq!(custom.review.diff_budget_chars, 5000);
        assert!(
            matches!(&err, ConfigError::InvalidValue { path, .. } if path == "review.diff_budget_chars"),
            "{err}"
        );
    }

    #[test]
    fn review_chunk_limits_have_defaults_and_zero_is_rejected() {
        // Arrange
        let set = "[review]\nmax_chunks = 2\nchunk_concurrency = 1\n";
        let no_chunks = "[review]\nmax_chunks = 0\n";
        let no_workers = "[review]\nchunk_concurrency = 0\n";

        // Act
        let default = Config::from_toml_str("").unwrap().config;
        let custom = Config::from_toml_str(set).unwrap().config;
        let chunks_err = Config::from_toml_str(no_chunks).unwrap_err();
        let workers_err = Config::from_toml_str(no_workers).unwrap_err();

        // Assert
        assert_eq!(default.review.max_chunks, 4);
        assert_eq!(default.review.chunk_concurrency, 3);
        assert_eq!(custom.review.max_chunks, 2);
        assert_eq!(custom.review.chunk_concurrency, 1);
        assert!(
            matches!(&chunks_err, ConfigError::InvalidValue { path, .. } if path == "review.max_chunks"),
            "{chunks_err}"
        );
        assert!(
            matches!(&workers_err, ConfigError::InvalidValue { path, .. } if path == "review.chunk_concurrency"),
            "{workers_err}"
        );
    }

    #[test]
    fn review_routing_rules_are_read_in_order_and_fallback_is_on_by_default() {
        // Arrange
        let set = "[review.routing]\nfallback = false\n\
            [[review.routing.rules]]\nmax_hunks = 3\nmodel = \"cheap\"\n\
            [[review.routing.rules]]\nmodel = \"strong\"\n";

        // Act
        let default = Config::from_toml_str("").unwrap().config;
        let custom = Config::from_toml_str(set).unwrap().config;

        // Assert
        assert_eq!(default.review.routing.rules, []);
        assert!(default.review.routing.fallback);
        assert!(!custom.review.routing.fallback);
        assert_eq!(custom.review.routing.rules.len(), 2);
        assert_eq!(custom.review.routing.rules[0].max_hunks, Some(3));
        assert_eq!(custom.review.routing.rules[1].model, "strong");
    }

    #[test]
    fn describe_labels_are_read_and_a_blank_or_repeated_one_is_rejected() {
        // Arrange
        let set = "[[describe.labels]]\nname = \"bug\"\nuse_when = \"the change fixes a defect\"\n";
        let no_name = "[[describe.labels]]\nname = \" \"\nuse_when = \"x\"\n";
        let no_line = "[[describe.labels]]\nname = \"bug\"\nuse_when = \"\"\n";
        let twice = "[[describe.labels]]\nname = \"Bug\"\nuse_when = \"x\"\n\
            [[describe.labels]]\nname = \"bug\"\nuse_when = \"y\"\n";

        // Act
        let default = Config::from_toml_str("").unwrap().config;
        let custom = Config::from_toml_str(set).unwrap().config;
        let errors = [no_name, no_line, twice].map(|text| Config::from_toml_str(text).unwrap_err());

        // Assert
        assert_eq!(default.describe.labels, []);
        assert_eq!(custom.describe.labels[0].name, "bug");
        assert_eq!(
            custom.describe.labels[0].use_when,
            "the change fixes a defect"
        );
        let paths: Vec<&str> = errors
            .iter()
            .map(|e| match e {
                ConfigError::InvalidValue { path, .. } => path.as_str(),
                other => panic!("{other}"),
            })
            .collect();
        assert_eq!(
            paths,
            [
                "describe.labels.0.name",
                "describe.labels.0.use_when",
                "describe.labels.1.name"
            ]
        );
    }

    #[test]
    fn review_check_commands_are_read_and_a_blank_one_is_rejected() {
        // Arrange
        let set = "[review]\ncheck_commands = [\"cargo test\", \"cargo clippy\"]\n";
        let blank = "[review]\ncheck_commands = [\"cargo test\", \"  \"]\n";

        // Act
        let default = Config::from_toml_str("").unwrap().config;
        let custom = Config::from_toml_str(set).unwrap().config;
        let err = Config::from_toml_str(blank).unwrap_err();

        // Assert
        assert_eq!(default.review.check_commands, Vec::<String>::new());
        assert_eq!(custom.review.check_commands, ["cargo test", "cargo clippy"]);
        assert!(
            matches!(&err, ConfigError::InvalidValue { path, .. } if path == "review.check_commands.1"),
            "{err}"
        );
    }

    #[test]
    fn a_review_routing_rule_with_an_empty_model_is_rejected() {
        // Arrange
        let text = "[[review.routing.rules]]\nmax_hunks = 3\nmodel = \"  \"\n";

        // Act
        let err = Config::from_toml_str(text).unwrap_err();

        // Assert
        assert!(
            matches!(&err, ConfigError::InvalidValue { path, .. } if path == "review.routing.rules.0.model"),
            "{err}"
        );
    }

    #[test]
    fn review_verifier_is_on_by_default_with_a_threshold_of_four_and_ten_is_the_top() {
        // Arrange
        let set = "[review]\nverify = false\nverify_threshold = 7\n";
        let too_high = "[review]\nverify_threshold = 11\n";

        // Act
        let default = Config::from_toml_str("").unwrap().config;
        let custom = Config::from_toml_str(set).unwrap().config;
        let err = Config::from_toml_str(too_high).unwrap_err();

        // Assert
        assert!(default.review.verify);
        assert_eq!(default.review.verify_threshold, 4);
        assert!(!custom.review.verify);
        assert_eq!(custom.review.verify_threshold, 7);
        assert!(
            matches!(&err, ConfigError::InvalidValue { path, .. } if path == "review.verify_threshold"),
            "{err}"
        );
    }

    #[test]
    fn review_guidance_defaults_to_500_lines_and_no_extra_instructions() {
        // Arrange
        let set = "[review]\nguidance_lines = 0\nextra_instructions = \"Check the locks.\"\n";

        // Act
        let default = Config::from_toml_str("").unwrap().config;
        let custom = Config::from_toml_str(set).unwrap().config;

        // Assert
        assert_eq!(default.review.guidance_lines, 500);
        assert_eq!(default.review.extra_instructions, "");
        assert_eq!(custom.review.guidance_lines, 0);
        assert_eq!(custom.review.extra_instructions, "Check the locks.");
    }

    #[test]
    fn review_ignore_and_skip_default_to_builtin_ignores_and_no_skips() {
        // Arrange and Act
        let review = Config::from_toml_str("").unwrap().config.review;

        // Assert
        assert!(review.ignore.builtin);
        assert_eq!(review.ignore.globs.len(), 0);
        assert_eq!(review.skip, crate::review::ReviewSkip::default());
        assert!(!review.skip.draft && !review.skip.bots);
    }

    #[test]
    fn review_ignore_and_skip_keys_are_read() {
        // Arrange
        let input = r#"
[review.ignore]
globs = ["docs/**"]
builtin = false

[review.skip]
draft = true
labels = ["wip"]
title_regex = ["^chore"]
source_branch_regex = ["^bot/"]
target_branch_regex = ["^release/"]
authors = ["alice"]
bots = true
"#;

        // Act
        let parsed = Config::from_toml_str(input).unwrap();

        // Assert
        let review = parsed.config.review;
        assert_eq!(review.ignore.globs, ["docs/**"]);
        assert!(!review.ignore.builtin);
        assert!(review.skip.draft && review.skip.bots);
        assert_eq!(review.skip.labels, ["wip"]);
        assert_eq!(review.skip.title_regex, ["^chore"]);
        assert_eq!(review.skip.source_branch_regex, ["^bot/"]);
        assert_eq!(review.skip.target_branch_regex, ["^release/"]);
        assert_eq!(review.skip.authors, ["alice"]);
        assert_eq!(parsed.unknown_keys.len(), 0);
    }

    #[test]
    fn the_terminal_notification_defaults_to_the_bell_and_is_read_from_the_file() {
        // Arrange
        let named = |mode: &str| format!("[tui]\nnotify = \"{mode}\"\n");

        // Act
        let default = Config::from_toml_str("").unwrap().config.tui.notify;
        let read: Vec<_> = ["off", "bell", "osc9", "osc777"]
            .iter()
            .map(|mode| {
                Config::from_toml_str(&named(mode))
                    .unwrap()
                    .config
                    .tui
                    .notify
            })
            .collect();

        // Assert
        assert_eq!(default, NotifyMode::Bell);
        assert_eq!(
            read,
            [
                NotifyMode::Off,
                NotifyMode::Bell,
                NotifyMode::Osc9,
                NotifyMode::Osc777
            ]
        );
    }

    #[test]
    fn the_cache_hint_defaults_are_read_and_overridden_from_the_file() {
        // Arrange
        let text = "[tui]\ncache_ttl_secs = 3600\ncache_hint_tokens = 1000\n";

        // Act
        let default = Config::from_toml_str("").unwrap().config.tui;
        let read = Config::from_toml_str(text).unwrap().config.tui;

        // Assert
        assert_eq!(default.cache_ttl_secs, DEFAULT_CACHE_TTL_SECS);
        assert_eq!(default.cache_hint_tokens, DEFAULT_CACHE_HINT_TOKENS);
        assert_eq!((read.cache_ttl_secs, read.cache_hint_tokens), (3600, 1000));
    }

    #[test]
    fn the_fetch_tool_is_off_until_a_host_is_listed() {
        // Act
        let fetch = Config::from_toml_str("").unwrap().config.fetch;

        // Assert
        assert_eq!(fetch.allow_hosts, Vec::<String>::new());
    }

    #[test]
    fn the_fetch_table_is_read_from_the_file() {
        // Arrange
        let text = "[fetch]\nallow_hosts = [\"docs.rs\", \"*.rust-lang.org\"]\nmax_bytes = 4096\ntimeout_ms = 2000\n";

        // Act
        let fetch = Config::from_toml_str(text).unwrap().config.fetch;

        // Assert
        assert_eq!(fetch.allow_hosts, ["docs.rs", "*.rust-lang.org"]);
        assert_eq!((fetch.max_bytes, fetch.timeout_ms), (4096, 2000));
    }

    #[test]
    fn a_misspelled_fetch_key_is_an_error_so_a_typo_cannot_widen_or_hide_the_list() {
        // Act
        let error = Config::from_toml_str("[fetch]\nallow_host = [\"docs.rs\"]\n").unwrap_err();

        // Assert
        assert!(error.to_string().contains("allow_host"), "{error}");
    }

    #[test]
    fn a_fetch_table_with_a_bad_value_names_the_key() {
        // Arrange
        let cases = [
            ("allow_hosts = [\"10.0.0.1\"]", "fetch.allow_hosts"),
            ("allow_hosts = [\"https://docs.rs\"]", "fetch.allow_hosts"),
            ("max_bytes = 0", "fetch.max_bytes"),
            ("max_bytes = 99999999", "fetch.max_bytes"),
            ("timeout_ms = 0", "fetch.timeout_ms"),
            ("timeout_ms = 99999999", "fetch.timeout_ms"),
        ];

        for (line, key) in cases {
            // Act
            let error = Config::from_toml_str(&format!("[fetch]\n{line}\n")).unwrap_err();

            // Assert
            assert!(error.to_string().contains(key), "{line}: {error}");
        }
    }

    #[test]
    fn recall_is_off_and_lists_no_repository_until_the_file_says_so() {
        // Act
        let recall = Config::from_toml_str("").unwrap().config.recall;

        // Assert
        assert!(!recall.enabled);
        assert_eq!(recall.delegated_repos, Vec::<String>::new());
    }

    #[test]
    fn the_recall_table_is_read_from_the_file() {
        // Arrange
        let text = "[recall]\nenabled = true\ndelegated_repos = [\"/work/repo\"]\n";

        // Act
        let recall = Config::from_toml_str(text).unwrap().config.recall;

        // Assert
        assert!(recall.enabled);
        assert_eq!(recall.delegated_repos, ["/work/repo"]);
    }

    #[test]
    fn a_misspelled_recall_key_or_a_relative_repository_is_an_error() {
        // Arrange
        let cases = [
            ("[recall]\nenable = true\n", "enable"),
            (
                "[recall]\ndelegated_repos = [\"repo\"]\n",
                "recall.delegated_repos",
            ),
            (
                "[recall]\ndelegated_repos = [\"\"]\n",
                "recall.delegated_repos",
            ),
        ];

        for (text, key) in cases {
            // Act
            let error = Config::from_toml_str(text).unwrap_err();

            // Assert
            assert!(error.to_string().contains(key), "{text}: {error}");
        }
    }

    #[test]
    fn the_repo_map_is_on_with_a_small_share_and_is_read_from_the_file() {
        // Arrange
        let text = "[repo_map]\nenabled = false\ntoken_share_percent = 5\n";

        // Act
        let default = Config::from_toml_str("").unwrap().config.repo_map;
        let read = Config::from_toml_str(text).unwrap().config.repo_map;

        // Assert
        assert!(default.enabled);
        assert_eq!(default.token_share_percent, 2);
        assert!(!read.enabled);
        assert_eq!(read.token_share_percent, 5);
    }

    #[test]
    fn a_repo_map_share_outside_one_to_twenty_or_a_misspelled_key_is_an_error() {
        for (text, key) in [
            (
                "[repo_map]\ntoken_share_percent = 0\n",
                "repo_map.token_share_percent",
            ),
            (
                "[repo_map]\ntoken_share_percent = 21\n",
                "repo_map.token_share_percent",
            ),
            ("[repo_map]\nenable = true\n", "enable"),
        ] {
            // Act
            let error = Config::from_toml_str(text).unwrap_err();

            // Assert
            assert!(error.to_string().contains(key), "{text}: {error}");
        }
    }

    #[test]
    fn no_tool_server_exists_until_the_file_lists_one() {
        // Act
        let servers = Config::from_toml_str("").unwrap().config.tool_servers;

        // Assert
        assert!(servers.is_empty());
    }

    #[test]
    fn a_tool_server_table_is_read_from_the_file() {
        // Arrange
        let text = "[tool_servers.notes]\ncommand = [\"/usr/bin/notes\", \"--stdio\"]\nenv = [\"NOTES_TOKEN\"]\ntimeout_ms = 5000\n\n[tool_servers.plain]\ncommand = [\"/bin/plain\"]\n";

        // Act
        let servers = Config::from_toml_str(text).unwrap().config.tool_servers;

        // Assert
        let notes = &servers["notes"];
        assert_eq!(notes.command, ["/usr/bin/notes", "--stdio"]);
        assert_eq!(notes.env, ["NOTES_TOKEN"]);
        assert_eq!(notes.timeout_ms, 5000);
        assert_eq!(servers["plain"].env, Vec::<String>::new());
        assert_eq!(servers["plain"].timeout_ms, 30_000);
    }

    #[test]
    fn a_bad_tool_server_table_names_the_key_that_is_wrong() {
        // Arrange
        let cases = [
            (
                "[tool_servers.Bad_Name]\ncommand = [\"/bin/x\"]\n",
                "tool_servers.Bad_Name",
            ),
            ("[tool_servers.a]\ncommand = []\n", "tool_servers.a.command"),
            (
                "[tool_servers.a]\ncommand = [\"x\"]\n",
                "tool_servers.a.command",
            ),
            (
                "[tool_servers.a]\ncommand = [\"/bin/x\"]\nenv = [\"1BAD\"]\n",
                "tool_servers.a.env",
            ),
            (
                "[tool_servers.a]\ncommand = [\"/bin/x\"]\ntimeout_ms = 0\n",
                "tool_servers.a.timeout_ms",
            ),
            (
                "[tool_servers.a]\ncommand = [\"/bin/x\"]\nshell = true\n",
                "shell",
            ),
        ];

        for (text, key) in cases {
            // Act
            let error = Config::from_toml_str(text).unwrap_err();

            // Assert
            assert!(error.to_string().contains(key), "{text}: {error}");
        }
    }

    #[test]
    fn the_worktree_table_defaults_to_no_folder_and_no_setup() {
        // Act
        let worktree = Config::from_toml_str("").unwrap().config.worktree;

        // Assert
        assert_eq!(worktree.dir, None);
        assert_eq!(worktree.setup, Vec::<String>::new());
    }

    #[test]
    fn the_worktree_folder_and_setup_command_are_read_from_the_file() {
        // Arrange
        let text = "[worktree]\ndir = \"/work/trees\"\nsetup = [\"npm\", \"install\"]\n";

        // Act
        let parsed = Config::from_toml_str(text).unwrap();

        // Assert
        assert_eq!(
            parsed.config.worktree.dir,
            Some(PathBuf::from("/work/trees"))
        );
        assert_eq!(parsed.config.worktree.setup, ["npm", "install"]);
        assert_eq!(parsed.unknown_keys, Vec::<String>::new());
    }

    #[test]
    fn worktree_cleanup_is_off_by_default_and_read_from_the_file() {
        // Arrange
        let text = "[worktree.cleanup]\nenabled = true\nidle_days = 3\nclean_command = [\"cargo\", \"clean\"]\n";

        // Act
        let default = Config::from_toml_str("").unwrap().config.worktree.cleanup;
        let read = Config::from_toml_str(text).unwrap().config.worktree.cleanup;

        // Assert
        assert!(!default.enabled);
        assert_eq!(default.idle_days, 14);
        assert_eq!(default.clean_command, Vec::<String>::new());
        assert!(read.enabled);
        assert_eq!(read.idle_days, 3);
        assert_eq!(read.clean_command, ["cargo", "clean"]);
    }

    #[test]
    fn a_zero_idle_period_or_an_empty_clean_program_is_rejected() {
        // Arrange
        let cases = [
            (
                "[worktree.cleanup]\nidle_days = 0\n",
                "worktree.cleanup.idle_days",
            ),
            (
                "[worktree.cleanup]\nclean_command = [\"\"]\n",
                "worktree.cleanup.clean_command",
            ),
        ];

        for (text, key) in cases {
            // Act
            let err = Config::from_toml_str(text).unwrap_err();

            // Assert
            assert!(err.to_string().contains(key), "{err}");
        }
    }

    #[test]
    fn checkpoint_pruning_is_off_by_default_and_read_from_the_file() {
        // Arrange
        let text = "[checkpoints.prune]\nenabled = true\nkeep_days = 10\nkeep_turns = 5\nrewind_days = 2\n";

        // Act
        let default = Config::from_toml_str("").unwrap().config.checkpoints.prune;
        let read = Config::from_toml_str(text)
            .unwrap()
            .config
            .checkpoints
            .prune;

        // Assert
        assert!(!default.enabled);
        assert_eq!(
            (default.keep_days, default.keep_turns, default.rewind_days),
            (30, 50, 7)
        );
        assert!(read.enabled);
        assert_eq!(
            (read.keep_days, read.keep_turns, read.rewind_days),
            (10, 5, 2)
        );
    }

    #[test]
    fn a_zero_pruning_limit_is_rejected_naming_the_key() {
        for key in ["keep_days", "keep_turns", "rewind_days"] {
            // Arrange
            let text = format!("[checkpoints.prune]\n{key} = 0\n");

            // Act
            let err = Config::from_toml_str(&text).unwrap_err();

            // Assert
            assert!(err.to_string().contains(key), "{err}");
        }
    }

    #[test]
    fn a_relative_worktree_folder_or_an_empty_setup_program_is_rejected() {
        // Arrange
        let cases = [
            ("[worktree]\ndir = \"trees\"\n", "worktree.dir"),
            ("[worktree]\nsetup = [\"\"]\n", "worktree.setup"),
        ];

        for (text, key) in cases {
            // Act
            let err = Config::from_toml_str(text).unwrap_err();

            // Assert
            assert!(err.to_string().contains(key), "{err}");
        }
    }

    #[test]
    fn an_unknown_terminal_notification_is_rejected_naming_the_key() {
        // Act
        let err = Config::from_toml_str("[tui]\nnotify = \"flash\"\n").unwrap_err();

        // Assert
        assert!(err.to_string().contains("notify"), "{err}");
    }

    #[test]
    fn loop_guard_thresholds_default_to_3_and_4() {
        // Act
        let guard = Config::from_toml_str("").unwrap().config.loop_guard;

        // Assert
        assert_eq!((guard.warn_after, guard.stop_after), (3, 4));
    }

    #[test]
    fn loop_guard_thresholds_are_read_from_the_file() {
        // Arrange
        let input = "[loop_guard]\nwarn_after = 2\nstop_after = 9\n";

        // Act
        let guard = Config::from_toml_str(input).unwrap().config.loop_guard;

        // Assert
        assert_eq!((guard.warn_after, guard.stop_after), (2, 9));
    }

    #[test]
    fn bad_loop_guard_thresholds_are_rejected_naming_the_key() {
        for (input, key) in [
            ("[loop_guard]\nwarn_after = 1\n", "loop_guard.warn_after"),
            ("[loop_guard]\nwarn_after = 0\n", "loop_guard.warn_after"),
            ("[loop_guard]\nstop_after = 3\n", "loop_guard.stop_after"),
            (
                "[loop_guard]\nwarn_after = 5\nstop_after = 4\n",
                "loop_guard.stop_after",
            ),
        ] {
            // Act
            let err = Config::from_toml_str(input).unwrap_err();

            // Assert
            assert!(
                matches!(&err, ConfigError::InvalidValue { path, .. } if path == key),
                "{input}: {err}"
            );
        }
    }

    #[test]
    fn a_zero_review_finding_cap_is_rejected_naming_the_key() {
        // Arrange
        let input = "[review]\nmax_findings = 0\n";

        // Act
        let err = Config::from_toml_str(input).unwrap_err();

        // Assert
        assert!(
            matches!(&err, ConfigError::InvalidValue { path, .. } if path == "review.max_findings"),
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
