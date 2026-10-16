// SPDX-License-Identifier: AGPL-3.0-only

//! Method names, parameters and results for requests between client and daemon.
//!
//! Structs tolerate unknown fields so older peers keep working, and optional
//! fields default so newer clients can omit them.

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
/// Liveness check.
pub const DAEMON_PING: &str = "daemon.ping";

/// Parameters of [`INITIALIZE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeParams {
    /// Protocol version the client speaks.
    pub protocol_version: u32,
    /// Name of the connecting client.
    pub client_name: String,
}

/// Result of [`INITIALIZE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeResult {
    /// Protocol version the daemon speaks.
    pub protocol_version: u32,
    /// Version of the daemon build.
    pub daemon_version: String,
}

/// Parameters of [`SESSION_CREATE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreateResult {
    /// The new session.
    pub session: SessionInfo,
}

/// Parameters of [`SESSION_SEND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSendParams {
    /// Target session.
    pub session_id: SessionId,
    /// User message text.
    pub text: String,
}

/// Result of [`SESSION_SEND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSendResult {
    /// The turn the message started.
    pub turn_id: TurnId,
}

/// Parameters of [`SESSION_CANCEL`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCancelParams {
    /// Target session.
    pub session_id: SessionId,
}

/// Result of [`SESSION_CANCEL`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCancelResult {
    /// Whether a running turn was cancelled.
    pub cancelled: bool,
}

/// Parameters of [`SESSION_SUBSCRIBE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSubscribeParams {
    /// Target session.
    pub session_id: SessionId,
    /// Replay events after this sequence number, or only new events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_seq: Option<u64>,
}

/// Result of [`SESSION_SUBSCRIBE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSubscribeResult {
    /// Whether the subscription is active.
    pub subscribed: bool,
}

/// Parameters of [`SESSION_RESUME`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResumeParams {
    /// Session to reopen.
    pub session_id: SessionId,
}

/// Result of [`SESSION_RESUME`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResumeResult {
    /// The reopened session.
    pub session: SessionInfo,
}

/// Parameters of [`PERMISSION_RESPOND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRespondParams {
    /// Session that raised the request.
    pub session_id: SessionId,
    /// The request being answered.
    pub request_id: RequestId,
    /// The decision.
    pub decision: PermissionDecision,
}

/// Result of [`PERMISSION_RESPOND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRespondResult {
    /// Whether the request was still pending and took the decision.
    pub accepted: bool,
}

/// Parameters of [`GRANT_ADD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantAddParams {
    /// Session the grant applies to.
    pub session_id: SessionId,
    /// Tool the grant covers.
    pub tool: String,
    /// Argument pattern limiting the grant, or all calls of the tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// Result of [`GRANT_ADD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantAddResult {
    /// Identifier of the stored grant.
    pub grant_id: String,
}

/// Parameters of [`USAGE_SUMMARY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSummaryParams {
    /// Session to summarize.
    pub session_id: SessionId,
}

/// Result of [`USAGE_SUMMARY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSummaryResult {
    /// Total usage of the session.
    pub usage: TokenUsage,
}

/// Parameters of [`SESSION_SET_MODEL`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSetModelParams {
    /// Target session.
    pub session_id: SessionId,
    /// Model to switch to.
    pub model: String,
}

/// Result of [`SESSION_SET_MODEL`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSetModelResult {
    /// The session after the switch.
    pub session: SessionInfo,
}

/// Parameters of [`DAEMON_STATUS`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatusParams {}

/// Result of [`DAEMON_STATUS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatusResult {
    /// Version of the daemon build.
    pub version: String,
    /// Seconds since the daemon started.
    pub uptime_secs: u64,
    /// Sessions currently loaded.
    pub sessions: Vec<SessionInfo>,
}

/// Parameters of [`DAEMON_PING`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingParams {}

/// Result of [`DAEMON_PING`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
}
