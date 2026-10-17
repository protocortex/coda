// SPDX-License-Identifier: AGPL-3.0-only

//! Method names, parameters and results for requests between client and daemon.
//!
//! Structs tolerate unknown fields so older peers keep working, and optional
//! fields default so newer clients can omit them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::{
    BackendKind, Mode, PermissionDecision, RequestId, SessionId, SessionInfo, TokenUsage, TurnId,
};

/// Protocol version announced and checked during `initialize`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Handshake that opens a connection.
pub const INITIALIZE: &str = "initialize";
/// Starts a session.
pub const SESSION_CREATE: &str = "session.create";
/// Sends a user message, starting a turn.
pub const SESSION_SEND: &str = "session.send";
/// Cancels the running turn.
pub const SESSION_CANCEL: &str = "session.cancel";
/// Starts streaming a session's events to this connection.
pub const SESSION_SUBSCRIBE: &str = "session.subscribe";
/// Reopens a stored session.
pub const SESSION_RESUME: &str = "session.resume";
/// Answers a pending permission request.
pub const PERMISSION_RESPOND: &str = "permission.respond";
/// Adds a standing permission grant.
pub const GRANT_ADD: &str = "grant.add";
/// Reports token usage.
pub const USAGE_SUMMARY: &str = "usage.summary";
/// Switches the model of a session.
pub const SESSION_SET_MODEL: &str = "session.set_model";
/// Reports daemon state.
pub const DAEMON_STATUS: &str = "daemon.status";
/// Asks the daemon to stop serving and exit.
pub const DAEMON_SHUTDOWN: &str = "daemon.shutdown";
/// Liveness check.
pub const DAEMON_PING: &str = "daemon.ping";

/// Every request method name; a test fails when a constant is missing here.
pub const METHODS: &[&str] = &[
    INITIALIZE,
    SESSION_CREATE,
    SESSION_SEND,
    SESSION_CANCEL,
    SESSION_SUBSCRIBE,
    SESSION_RESUME,
    PERMISSION_RESPOND,
    GRANT_ADD,
    USAGE_SUMMARY,
    SESSION_SET_MODEL,
    DAEMON_STATUS,
    DAEMON_SHUTDOWN,
    DAEMON_PING,
];

/// Parameters of [`INITIALIZE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InitializeParams {
    /// Protocol version the client speaks.
    pub protocol_version: u32,
    /// Name of the connecting client.
    pub client_name: String,
}

/// Result of [`INITIALIZE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InitializeResult {
    /// Protocol version the daemon speaks.
    pub protocol_version: u32,
    /// Version of the daemon build.
    pub daemon_version: String,
}

/// Parameters of [`SESSION_CREATE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCreateParams {
    /// Backend to drive the session.
    pub backend: BackendKind,
    /// Working directory of the session.
    pub cwd: String,
    /// Model to use, or the backend default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Initial autonomy mode.
    #[serde(default)]
    pub mode: Mode,
}

/// Result of [`SESSION_CREATE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCreateResult {
    /// The new session.
    pub session: SessionInfo,
}

/// Parameters of [`SESSION_SEND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSendParams {
    /// Target session.
    pub session_id: SessionId,
    /// User message text.
    pub text: String,
}

/// Result of [`SESSION_SEND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSendResult {
    /// The turn the message started.
    pub turn_id: TurnId,
}

/// Parameters of [`SESSION_CANCEL`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCancelParams {
    /// Target session.
    pub session_id: SessionId,
}

/// Result of [`SESSION_CANCEL`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCancelResult {
    /// Whether a running turn was cancelled.
    pub cancelled: bool,
}

/// Parameters of [`SESSION_SUBSCRIBE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSubscribeParams {
    /// Target session.
    pub session_id: SessionId,
    /// Replay events after this sequence number, or only new events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_seq: Option<u64>,
}

/// Result of [`SESSION_SUBSCRIBE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSubscribeResult {
    /// Whether the subscription is active.
    pub subscribed: bool,
}

/// Parameters of [`SESSION_RESUME`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionResumeParams {
    /// Session to reopen.
    pub session_id: SessionId,
}

/// Result of [`SESSION_RESUME`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionResumeResult {
    /// The reopened session.
    pub session: SessionInfo,
}

/// Parameters of [`PERMISSION_RESPOND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionRespondParams {
    /// Session that raised the request.
    pub session_id: SessionId,
    /// The request being answered.
    pub request_id: RequestId,
    /// The decision.
    pub decision: PermissionDecision,
}

/// Result of [`PERMISSION_RESPOND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionRespondResult {
    /// Whether the request was still pending and took the decision.
    pub accepted: bool,
}

/// Parameters of [`GRANT_ADD`]. A grant matches only when repository, tool,
/// cwd and pattern all equal the requested call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GrantAddParams {
    /// Canonical root of the git repository the grant applies to.
    pub repo_path: String,
    /// Tool the grant covers.
    pub tool: String,
    /// Working directory the grant applies to.
    pub cwd: String,
    /// Exact command or argument pattern the grant covers.
    pub pattern: String,
}

/// Result of [`GRANT_ADD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GrantAddResult {
    /// When the grant was first stored, in milliseconds since the Unix epoch.
    pub granted_at: i64,
}

/// Parameters of [`USAGE_SUMMARY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UsageSummaryParams {
    /// Session to summarize, or every session when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

/// Result of [`USAGE_SUMMARY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UsageSummaryResult {
    /// Total usage of the summarized sessions.
    pub usage: TokenUsage,
    /// Bytes of tool output the token economy kept out of the context.
    #[serde(default)]
    pub saved_bytes: u64,
}

/// Parameters of [`SESSION_SET_MODEL`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSetModelParams {
    /// Target session.
    pub session_id: SessionId,
    /// Model to switch to.
    pub model: String,
}

/// Result of [`SESSION_SET_MODEL`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSetModelResult {
    /// The session after the switch.
    pub session: SessionInfo,
}

/// Parameters of [`DAEMON_STATUS`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DaemonStatusParams {}

/// Result of [`DAEMON_STATUS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DaemonStatusResult {
    /// Process id of the daemon.
    pub pid: u32,
    /// Protocol version the daemon speaks.
    pub protocol_version: u32,
    /// Version of the daemon build.
    pub version: String,
    /// Seconds since the daemon started.
    pub uptime_secs: u64,
    /// Connections open right now, including the one asking.
    pub client_count: u32,
    /// Turns running right now.
    pub active_turns: u32,
    /// Directory holding the daemon's database.
    pub data_dir: String,
}

/// Parameters of [`DAEMON_SHUTDOWN`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DaemonShutdownParams {}

/// Result of [`DAEMON_SHUTDOWN`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DaemonShutdownResult {}

/// Parameters of [`DAEMON_PING`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PingParams {}

/// Result of [`DAEMON_PING`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PingResult {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_constants_match_the_contract() {
        // Arrange
        let expected = [
            (INITIALIZE, "initialize"),
            (SESSION_CREATE, "session.create"),
            (SESSION_SEND, "session.send"),
            (SESSION_CANCEL, "session.cancel"),
            (SESSION_SUBSCRIBE, "session.subscribe"),
            (SESSION_RESUME, "session.resume"),
            (PERMISSION_RESPOND, "permission.respond"),
            (GRANT_ADD, "grant.add"),
            (USAGE_SUMMARY, "usage.summary"),
            (SESSION_SET_MODEL, "session.set_model"),
            (DAEMON_STATUS, "daemon.status"),
            (DAEMON_SHUTDOWN, "daemon.shutdown"),
            (DAEMON_PING, "daemon.ping"),
            (crate::events::EVENT_NOTIFICATION, "session.event"),
        ];

        for (constant, name) in expected {
            // Act and Assert
            assert_eq!(constant, name);
        }
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn optional_params_default_when_absent() {
        // Arrange
        let raw = r#"{"backend":"faux","cwd":"/work"}"#;

        // Act
        let params: SessionCreateParams = serde_json::from_str(raw).unwrap();

        // Assert
        assert_eq!(params.model, None);
        assert_eq!(params.mode, crate::types::Mode::Default);
    }

    #[test]
    fn methods_has_no_duplicates() {
        // Arrange
        let unique: std::collections::BTreeSet<&str> = METHODS.iter().copied().collect();

        // Act and Assert
        assert_eq!(unique.len(), METHODS.len());
    }

    #[test]
    fn methods_lists_every_method_constant() {
        // Arrange
        let declared: std::collections::BTreeSet<&str> = include_str!("methods.rs")
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("pub const ")?;
                let (_, value) = rest.split_once(": &str = \"")?;
                value.strip_suffix("\";")
            })
            .collect();

        // Act
        let listed: std::collections::BTreeSet<&str> = METHODS.iter().copied().collect();

        // Assert
        assert_eq!(listed, declared);
    }
}
