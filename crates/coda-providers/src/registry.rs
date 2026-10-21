// SPDX-License-Identifier: AGPL-3.0-only

//! The model providers and aliases a configuration defines, and the clients
//! built from them.
//!
//! The registry is a snapshot of `[providers.*]` and `[models.*]` taken when
//! it is built. A running daemon does not reread `config.toml`, so edits to
//! providers or aliases apply after the daemon restarts. A session stores a
//! model id, never an alias name: a switch resolves the alias once, so editing
//! the aliases later cannot change what a stored session runs. API keys are never stored: only the name of the
//! variable that holds one is kept, and the variable is read on each call.

use std::collections::BTreeMap;

use coda_config::config::{Config, ModelAlias, ProviderConfig};
use coda_protocol::types::BackendKind;

use crate::anthropic::Timing;
use crate::anthropic::provider::{AnthropicOptions, AnthropicProvider};
use crate::anthropic::request::RequestSettings;
use crate::model::{
    ModelProvider, ModelRequest, ModelStream, ProviderError, SystemClock, TokioSleeper,
};
use crate::models;
use crate::openai_compat::provider::{OpenAiCompatOptions, OpenAiCompatProvider};

/// Config key of the Anthropic provider section.
pub const ANTHROPIC_KEY: &str = "anthropic";
/// Config key of the OpenAI-compatible provider section.
pub const OPENAI_COMPAT_KEY: &str = "openai_compat";
/// Config key of the scripted test provider section.
pub const FAUX_KEY: &str = "faux";

const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-5-5";
const MAX_OUTPUT_TOKENS: u32 = 8192;

/// Why a model could not be selected or its client could not be built.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("unknown model alias `{0}`; define it under [models.{0}]")]
    UnknownAlias(String),
    #[error("model alias `{alias}` names the unknown provider `{provider}`")]
    UnknownProvider { alias: String, provider: String },
    #[error(
        "model alias `{alias}` belongs to provider `{provider}`, not to this session's backend"
    )]
    WrongBackend {
        alias: String,
        provider: &'static str,
    },
    #[error("provider {0} is disabled; set providers.{0}.enabled = true to use it")]
    Disabled(&'static str),
    #[error("no model chosen; set providers.{0}.model or switch to a model alias")]
    NoModel(&'static str),
    #[error("provider {0} needs providers.{0}.base_url")]
    NoBaseUrl(&'static str),
    #[error("backend {0:?} is not a model-API backend")]
    NotAModelBackend(BackendKind),
    #[error("the provider cannot be used: {0}")]
    Provider(#[from] ProviderError),
}

/// A backend and the model to run on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTarget {
    /// The backend that serves the model.
    pub backend: BackendKind,
    /// The model id sent to the provider.
    pub model: String,
}

/// A client for one model-API provider.
#[derive(Debug)]
pub enum ModelClient {
    Anthropic(AnthropicProvider<TokioSleeper, SystemClock>),
    OpenAiCompat(OpenAiCompatProvider<TokioSleeper, SystemClock>),
}

impl ModelProvider for ModelClient {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProviderError> {
        match self {
            Self::Anthropic(provider) => provider.stream(request).await,
            Self::OpenAiCompat(provider) => provider.stream(request).await,
        }
    }
}

/// The providers and aliases of one configuration.
#[derive(Debug, Clone)]
pub struct ModelRegistry {
    providers: BTreeMap<String, ProviderConfig>,
    aliases: BTreeMap<String, ModelAlias>,
    timing: Timing,
}

/// The provider section key of a backend.
fn provider_key(backend: BackendKind) -> Option<&'static str> {
    match backend {
        BackendKind::Anthropic => Some(ANTHROPIC_KEY),
        BackendKind::OpenaiCompat => Some(OPENAI_COMPAT_KEY),
        BackendKind::Faux => Some(FAUX_KEY),
        _ => None,
    }
}

const MODEL_BACKENDS: [BackendKind; 3] = [
    BackendKind::Anthropic,
    BackendKind::OpenaiCompat,
    BackendKind::Faux,
];

fn backend_of(provider: &str) -> Option<BackendKind> {
    MODEL_BACKENDS
        .into_iter()
        .find(|backend| provider_key(*backend) == Some(provider))
}

impl ModelRegistry {
    /// Takes the providers and aliases out of `config`.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            providers: config.providers.clone(),
            aliases: config.models.clone(),
            timing: Timing::from_config(&config.timing),
        }
    }

    /// The model-API backends whose provider section is enabled.
    #[must_use]
    pub fn enabled_backends(&self) -> Vec<BackendKind> {
        MODEL_BACKENDS
            .into_iter()
            .filter(|backend| {
                provider_key(*backend)
                    .and_then(|key| self.providers.get(key))
                    .is_some_and(|section| section.enabled)
            })
            .collect()
    }

    /// The target of the alias `name` on a session that runs on `backend`.
    ///
    /// # Errors
    /// Returns [`RegistryError::UnknownAlias`] when no such alias is defined,
    /// [`RegistryError::UnknownProvider`] when it names no known provider, and
    /// [`RegistryError::WrongBackend`] when its provider is not `backend`'s.
    pub fn alias(&self, name: &str, backend: BackendKind) -> Result<ModelTarget, RegistryError> {
        let alias = self
            .aliases
            .get(name)
            .ok_or_else(|| RegistryError::UnknownAlias(name.to_owned()))?;
        let served_by =
            backend_of(&alias.provider).ok_or_else(|| RegistryError::UnknownProvider {
                alias: name.to_owned(),
                provider: alias.provider.clone(),
            })?;
        if served_by != backend {
            return Err(RegistryError::WrongBackend {
                alias: name.to_owned(),
                provider: provider_key(served_by).unwrap_or_default(),
            });
        }
        Ok(ModelTarget {
            backend,
            model: alias.model.clone(),
        })
    }

    /// The target of the model a session stored: a model id, or `None` for the
    /// provider's default. The id is used as stored, even when an alias of the
    /// same name exists.
    ///
    /// # Errors
    /// Returns [`RegistryError::NotAModelBackend`] for a backend without a provider
    /// section, and [`RegistryError::NoModel`] when nothing names a model.
    pub fn select(
        &self,
        backend: BackendKind,
        stored: Option<&str>,
    ) -> Result<ModelTarget, RegistryError> {
        let key = provider_key(backend).ok_or(RegistryError::NotAModelBackend(backend))?;
        let model = match stored {
            Some(model) => model.to_owned(),
            None => self
                .providers
                .get(key)
                .and_then(|section| section.model.clone())
                .or_else(|| {
                    (backend == BackendKind::Anthropic).then(|| DEFAULT_ANTHROPIC_MODEL.to_owned())
                })
                .ok_or(RegistryError::NoModel(key))?,
        };
        Ok(ModelTarget { backend, model })
    }

    /// The context window of `target` in tokens: the `context_window` of an
    /// alias for the same provider and model, else the built-in table, else
    /// `None` for a model nobody knows.
    #[must_use]
    pub fn context_window(&self, target: &ModelTarget) -> Option<u32> {
        let provider = provider_key(target.backend);
        self.aliases
            .values()
            .find(|alias| {
                provider == Some(alias.provider.as_str())
                    && alias.model == target.model
                    && alias.context_window.is_some()
            })
            .and_then(|alias| alias.context_window)
            .or_else(|| models::context_window(&target.model))
    }

    /// Builds the client for `target`. `env` looks up the API key variable the
    /// provider names, on every call the client makes.
    ///
    /// # Errors
    /// Returns [`RegistryError::NotAModelBackend`] for a target without an HTTP
    /// client, [`RegistryError::Disabled`] when its section is off,
    /// [`RegistryError::NoBaseUrl`] when an OpenAI-compatible section has no
    /// URL, and [`RegistryError::Provider`] when the Anthropic key variable is
    /// unset or the client is refused.
    pub fn build(
        &self,
        target: &ModelTarget,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Result<ModelClient, RegistryError> {
        let key = provider_key(target.backend)
            .filter(|key| *key != FAUX_KEY)
            .ok_or(RegistryError::NotAModelBackend(target.backend))?;
        let section = self
            .providers
            .get(key)
            .filter(|section| section.enabled)
            .ok_or(RegistryError::Disabled(key))?;
        let settings = RequestSettings {
            model: target.model.clone(),
            max_tokens: MAX_OUTPUT_TOKENS,
            system: String::new(),
            tools: Vec::new(),
        };
        if target.backend == BackendKind::Anthropic {
            let options = AnthropicOptions {
                base_url: section
                    .base_url
                    .clone()
                    .unwrap_or_else(|| ANTHROPIC_BASE_URL.to_owned()),
                settings,
                timing: self.timing,
            };
            return Ok(ModelClient::Anthropic(AnthropicProvider::from_config(
                section, options, env,
            )?));
        }
        let base_url = section
            .base_url
            .clone()
            .ok_or(RegistryError::NoBaseUrl(key))?;
        let options = OpenAiCompatOptions {
            base_url,
            settings,
            timing: self.timing,
        };
        let provider = OpenAiCompatProvider::new(
            options,
            section.api_key_env.clone(),
            env,
            TokioSleeper,
            SystemClock,
        )?;
        Ok(ModelClient::OpenAiCompat(provider))
    }
}

#[cfg(test)]
mod tests {
    use coda_config::config::Config;
    use coda_protocol::types::BackendKind;

    use super::{ModelRegistry, ModelTarget, RegistryError};

    const CONFIG: &str = r#"
[providers.anthropic]
enabled = true
api_key_env = "TEAM_ANTHROPIC_KEY"
model = "claude-default"

[providers.openai_compat]
enabled = true
api_key_env = "LOCAL_LLM_KEY"
base_url = "http://localhost:11434/v1"

[providers.faux]
enabled = true

[models.fast]
provider = "anthropic"
model = "claude-fast"

[models.local]
provider = "openai_compat"
model = "qwen-coder"

[models.scripted]
provider = "faux"
model = "script-1"

[models.lost]
provider = "nowhere"
model = "m"
"#;

    #[test]
    fn only_enabled_providers_are_listed_as_backends() {
        // Arrange
        let all = registry(CONFIG);
        let one_off = registry(
            "[providers.openai_compat]\nenabled = false\n[providers.faux]\nenabled = true\n",
        );

        // Act
        let (listed, partial) = (all.enabled_backends(), one_off.enabled_backends());

        // Assert
        assert_eq!(
            listed,
            [
                BackendKind::Anthropic,
                BackendKind::OpenaiCompat,
                BackendKind::Faux
            ]
        );
        assert_eq!(partial, [BackendKind::Faux]);
    }

    fn registry(text: &str) -> ModelRegistry {
        ModelRegistry::from_config(&Config::from_toml_str(text).unwrap().config)
    }

    fn lookup(name: &str) -> Option<String> {
        (name == "TEAM_ANTHROPIC_KEY").then(|| "sk-secret-value".to_owned())
    }

    fn target(backend: BackendKind, model: &str) -> ModelTarget {
        ModelTarget {
            backend,
            model: model.to_owned(),
        }
    }

    #[test]
    fn a_known_model_reports_its_published_window_and_an_unknown_one_none() {
        // Arrange
        let registry = registry(CONFIG);
        let cases = [
            ("claude-sonnet-5-5", Some(200_000)),
            ("claude-sonnet-4-5", Some(200_000)),
            ("gpt-4o", Some(128_000)),
            ("claude-fast", None),
        ];

        for (model, expected) in cases {
            // Act
            let window = registry.context_window(&target(BackendKind::Anthropic, model));

            // Assert
            assert_eq!(window, expected, "{model}");
        }
    }

    #[test]
    fn an_alias_window_overrides_the_table_for_its_provider_and_model() {
        // Arrange
        let registry = registry(
            "[providers.anthropic]\nenabled = true\n\
             [models.wide]\nprovider = \"anthropic\"\nmodel = \"claude-sonnet-5-5\"\ncontext_window = 1000000\n\
             [models.mine]\nprovider = \"openai_compat\"\nmodel = \"qwen\"\ncontext_window = 32000\n",
        );

        // Act
        let (wide, mine, other_backend) = (
            registry.context_window(&target(BackendKind::Anthropic, "claude-sonnet-5-5")),
            registry.context_window(&target(BackendKind::OpenaiCompat, "qwen")),
            registry.context_window(&target(BackendKind::OpenaiCompat, "claude-sonnet-5-5")),
        );

        // Assert
        assert_eq!(wide, Some(1_000_000));
        assert_eq!(mine, Some(32_000));
        assert_eq!(other_backend, Some(200_000), "the override is per provider");
    }

    #[test]
    fn an_alias_without_a_window_keeps_the_table_value() {
        // Arrange
        let registry = registry("[models.plain]\nprovider = \"anthropic\"\nmodel = \"gpt-4o\"\n");

        // Act
        let window = registry.context_window(&target(BackendKind::Anthropic, "gpt-4o"));

        // Assert
        assert_eq!(window, Some(128_000));
    }

    #[test]
    fn an_alias_resolves_to_its_backend_and_model() {
        // Arrange
        let registry = registry(CONFIG);
        let cases = [
            ("fast", BackendKind::Anthropic, "claude-fast"),
            ("local", BackendKind::OpenaiCompat, "qwen-coder"),
            ("scripted", BackendKind::Faux, "script-1"),
        ];

        for (alias, backend, model) in cases {
            // Act
            let target = registry.alias(alias, backend).unwrap();

            // Assert
            assert_eq!((target.backend, target.model.as_str()), (backend, model));
        }
    }

    #[test]
    fn an_unknown_alias_is_rejected_by_name() {
        // Arrange
        let registry = registry(CONFIG);

        // Act
        let error = registry.alias("nope", BackendKind::Anthropic).unwrap_err();

        // Assert
        assert!(matches!(error, RegistryError::UnknownAlias(ref a) if a == "nope"));
        assert_eq!(
            error.to_string(),
            "unknown model alias `nope`; define it under [models.nope]"
        );
    }

    #[test]
    fn an_alias_for_another_backend_or_an_unknown_provider_is_rejected() {
        // Arrange
        let registry = registry(CONFIG);

        // Act
        let other_backend = registry.alias("local", BackendKind::Anthropic).unwrap_err();
        let no_provider = registry.alias("lost", BackendKind::Anthropic).unwrap_err();

        // Assert
        assert!(matches!(other_backend, RegistryError::WrongBackend { .. }));
        assert!(matches!(no_provider, RegistryError::UnknownProvider { .. }));
    }

    #[test]
    fn a_session_model_is_a_stored_id_even_if_an_alias_has_that_name_or_the_provider_default() {
        // Arrange
        let registry = registry(CONFIG);

        // Act
        let same_name_as_alias = registry
            .select(BackendKind::Anthropic, Some("fast"))
            .unwrap();
        let literal = registry
            .select(BackendKind::Anthropic, Some("claude-x"))
            .unwrap();
        let default = registry.select(BackendKind::Anthropic, None).unwrap();
        let no_default = registry
            .select(BackendKind::OpenaiCompat, None)
            .unwrap_err();

        // Assert
        assert_eq!(same_name_as_alias.model, "fast");
        assert_eq!(literal.model, "claude-x");
        assert_eq!(default.model, "claude-default");
        assert!(matches!(no_default, RegistryError::NoModel(_)));
    }

    #[test]
    fn a_missing_key_variable_names_only_that_provider_s_variable() {
        // Arrange
        let registry = registry(CONFIG);
        let target = registry.alias("fast", BackendKind::Anthropic).unwrap();

        // Act
        let error = registry.build(&target, |_| None).unwrap_err().to_string();

        // Assert
        assert!(error.contains("TEAM_ANTHROPIC_KEY"), "{error}");
        assert!(!error.contains("LOCAL_LLM_KEY"), "{error}");
    }

    #[test]
    fn a_built_client_never_shows_the_key_value() {
        // Arrange
        let registry = registry(CONFIG);
        let target = registry.alias("fast", BackendKind::Anthropic).unwrap();

        // Act
        let client = registry.build(&target, lookup).unwrap();

        // Assert
        assert!(!format!("{client:?}").contains("sk-secret-value"));
    }

    #[test]
    fn a_disabled_provider_or_missing_base_url_cannot_be_built() {
        // Arrange
        let off = registry("[providers.anthropic]\nenabled = false\napi_key_env = \"K\"\n");
        let bare = registry("[providers.openai_compat]\nenabled = true\n");
        let anthropic = off.select(BackendKind::Anthropic, Some("m")).unwrap();
        let compat = bare.select(BackendKind::OpenaiCompat, Some("m")).unwrap();

        // Act
        let disabled = off.build(&anthropic, lookup).unwrap_err();
        let no_url = bare.build(&compat, lookup).unwrap_err();

        // Assert
        assert!(matches!(disabled, RegistryError::Disabled(_)));
        assert!(matches!(no_url, RegistryError::NoBaseUrl(_)));
    }

    #[test]
    fn a_local_server_needs_no_key() {
        // Arrange
        let registry = registry(CONFIG);
        let target = registry.alias("local", BackendKind::OpenaiCompat).unwrap();

        // Act
        let client = registry.build(&target, |_| None);

        // Assert
        assert!(client.is_ok());
    }
}
