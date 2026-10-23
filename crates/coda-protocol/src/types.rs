// SPDX-License-Identifier: AGPL-3.0-only

//! Identifiers and value types shared by every protocol method and event.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wraps an existing identifier string.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// The identifier as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(
    /// Identifies one agent session.
    SessionId
);
string_id!(
    /// Identifies one turn within a session.
    TurnId
);
string_id!(
    /// Identifies one pending permission request.
    RequestId
);
string_id!(
    /// Identifies a stored original tool output.
    OriginalId
);

/// The agent backend that drives a session.
///
/// Adding a variant requires a `PROTOCOL_VERSION` bump: older peers reject
/// unknown variants rather than ignoring them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BackendKind {
    /// Claude Code CLI.
    ClaudeCode,
    /// Codex through its app server.
    CodexAppServer,
    /// Codex through one-shot exec runs.
    CodexExec,
    /// Anthropic API.
    Anthropic,
    /// Any API compatible with the `OpenAI` chat format.
    OpenaiCompat,
    /// Scripted backend used in tests.
    Faux,
}

/// The answer to a permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PermissionDecision {
    /// Allow this one call.
    AllowOnce,
    /// Allow this call and matching calls later.
    AllowAlways,
    /// Refuse the call.
    Deny,
}

/// How much autonomy the agent has in a session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Mode {
    /// Ask before risky actions.
    #[default]
    Default,
    /// Plan only, make no changes.
    Plan,
    /// Apply file edits without asking.
    AcceptEdits,
}

/// Token counts for a turn or a whole session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct TokenUsage {
    /// Tokens sent to the model.
    pub input_tokens: u64,
    /// Tokens produced by the model.
    pub output_tokens: u64,
    /// Input tokens served from the prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to the prompt cache.
    pub cache_write_tokens: u64,
}

/// A session as listed to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    /// Session identifier.
    pub id: SessionId,
    /// Backend driving the session.
    pub backend: BackendKind,
    /// Model in use, when one is selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Working directory of the session.
    pub cwd: String,
    /// Current autonomy mode.
    #[serde(default)]
    pub mode: Mode,
}

impl BackendKind {
    /// Whether coda runs the model loop and its tools itself, so that the
    /// user's hooks apply. Claude Code and Codex run their own tools.
    #[must_use]
    pub const fn is_model_backend(self) -> bool {
        match self {
            Self::Anthropic | Self::OpenaiCompat | Self::Faux => true,
            Self::ClaudeCode | Self::CodexAppServer | Self::CodexExec => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_the_backends_coda_drives_itself_are_model_backends() {
        // Arrange
        let cases = [
            (BackendKind::Anthropic, true),
            (BackendKind::OpenaiCompat, true),
            (BackendKind::Faux, true),
            (BackendKind::ClaudeCode, false),
            (BackendKind::CodexAppServer, false),
            (BackendKind::CodexExec, false),
        ];

        // Act and Assert
        for (kind, model) in cases {
            assert_eq!(kind.is_model_backend(), model, "{kind:?}");
        }
    }

    #[test]
    fn ids_serialize_as_bare_strings() {
        // Arrange
        let session = SessionId::new("s-1");
        let turn = TurnId::new("t-1");
        let request = RequestId::new("r-1");

        // Act
        let value = serde_json::to_value((&session, &turn, &request)).unwrap();

        // Assert
        assert_eq!(value, json!(["s-1", "t-1", "r-1"]));
        assert_eq!(session.as_str(), "s-1");
    }

    #[test]
    fn backend_kind_uses_snake_case_names() {
        // Arrange
        let cases = [
            (BackendKind::ClaudeCode, "claude_code"),
            (BackendKind::CodexAppServer, "codex_app_server"),
            (BackendKind::CodexExec, "codex_exec"),
            (BackendKind::Anthropic, "anthropic"),
            (BackendKind::OpenaiCompat, "openai_compat"),
            (BackendKind::Faux, "faux"),
        ];

        for (kind, name) in cases {
            // Act
            let value = serde_json::to_value(kind).unwrap();
            let parsed: BackendKind = serde_json::from_value(json!(name)).unwrap();

            // Assert
            assert_eq!(value, json!(name));
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn permission_decision_uses_snake_case_names() {
        // Arrange
        let cases = [
            (PermissionDecision::AllowOnce, "allow_once"),
            (PermissionDecision::AllowAlways, "allow_always"),
            (PermissionDecision::Deny, "deny"),
        ];

        for (decision, name) in cases {
            // Act
            let value = serde_json::to_value(decision).unwrap();

            // Assert
            assert_eq!(value, json!(name));
        }
    }

    #[test]
    fn mode_uses_snake_case_names() {
        // Arrange
        let cases = [
            (Mode::Default, "default"),
            (Mode::Plan, "plan"),
            (Mode::AcceptEdits, "accept_edits"),
        ];

        for (mode, name) in cases {
            // Act
            let value = serde_json::to_value(mode).unwrap();

            // Assert
            assert_eq!(value, json!(name));
        }
    }

    #[test]
    fn token_usage_defaults_missing_fields_and_ignores_unknown_ones() {
        // Arrange
        let raw = json!({"input_tokens": 10, "output_tokens": 4, "future_field": true});

        // Act
        let usage: TokenUsage = serde_json::from_value(raw).unwrap();

        // Assert
        assert_eq!(
            usage,
            TokenUsage {
                input_tokens: 10,
                output_tokens: 4,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }
        );
    }

    #[test]
    fn session_info_matches_golden_and_ignores_unknown_fields() {
        // Arrange
        let golden = json!({
            "id": "s-1",
            "backend": "faux",
            "model": "m-1",
            "cwd": "/work",
            "mode": "plan",
            "added_later": 1
        });

        // Act
        let info: SessionInfo = serde_json::from_value(golden).unwrap();

        // Assert
        assert_eq!(info.id, SessionId::new("s-1"));
        assert_eq!(info.backend, BackendKind::Faux);
        assert_eq!(info.model.as_deref(), Some("m-1"));
        assert_eq!(info.cwd, "/work");
        assert_eq!(info.mode, Mode::Plan);
    }

    #[test]
    fn session_info_defaults_optional_fields() {
        // Arrange
        let raw = json!({"id": "s-1", "backend": "faux", "cwd": "/work"});

        // Act
        let info: SessionInfo = serde_json::from_value(raw).unwrap();

        // Assert
        assert_eq!(info.model, None);
        assert_eq!(info.mode, Mode::Default);
    }
}
