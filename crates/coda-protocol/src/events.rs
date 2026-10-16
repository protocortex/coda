// SPDX-License-Identifier: AGPL-3.0-only

//! Notifications the daemon pushes to subscribed clients.
//!
//! Every variant except [`Event::StreamDelta`] is content-free: it names
//! sessions, turns and tools but never prompts, arguments or outputs.

use serde::{Deserialize, Serialize};

use crate::types::{RequestId, SessionId, TokenUsage, TurnId};

/// Notification method that wraps an [`Event`].
pub const EVENT_NOTIFICATION: &str = "session.event";

/// One event of a session.
///
/// Adding a variant requires a `PROTOCOL_VERSION` bump: older peers reject
/// unknown variants rather than ignoring them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    /// A permission request was approved.
    Approve {
        /// Session that raised the request.
        session_id: SessionId,
        /// The request.
        request_id: RequestId,
        /// Tool the request was for.
        tool: String,
    },
    /// A permission request was denied.
    Deny {
        /// Session that raised the request.
        session_id: SessionId,
        /// The request.
        request_id: RequestId,
        /// Tool the request was for.
        tool: String,
    },
    /// A permission request timed out without an answer.
    Timeout {
        /// Session that raised the request.
        session_id: SessionId,
        /// The request.
        request_id: RequestId,
        /// Tool the request was for.
        tool: String,
    },
    /// A turn finished.
    Result {
        /// Session of the turn.
        session_id: SessionId,
        /// The finished turn.
        turn_id: TurnId,
        /// Whether the turn ended in an error.
        is_error: bool,
    },
    /// Token usage reported for a turn.
    Usage {
        /// Session of the turn.
        session_id: SessionId,
        /// The turn the usage belongs to.
        turn_id: TurnId,
        /// Tokens used.
        usage: TokenUsage,
    },
    /// Approvals cannot be answered, so risky calls are refused.
    ApprovalsUnavailable {
        /// Affected session.
        session_id: SessionId,
        /// Why no approver is reachable.
        reason: String,
    },
    /// A piece of streamed model text.
    StreamDelta {
        /// Session of the turn.
        session_id: SessionId,
        /// Turn being streamed.
        turn_id: TurnId,
        /// The text fragment.
        text: String,
    },
    /// The backend process ended unexpectedly.
    BackendCrashed {
        /// Affected session.
        session_id: SessionId,
        /// Exit code, when the process exited normally enough to have one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// A running turn was cut short.
    TurnInterrupted {
        /// Session of the turn.
        session_id: SessionId,
        /// The interrupted turn.
        turn_id: TurnId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RequestId, SessionId, TurnId};
    use serde_json::json;

    #[test]
    fn approval_outcomes_carry_only_ids_and_tool_name() {
        // Arrange
        let event = Event::Deny {
            session_id: SessionId::new("s-1"),
            request_id: RequestId::new("r-1"),
            tool: "Bash".to_owned(),
        };

        // Act
        let value = serde_json::to_value(&event).unwrap();

        // Assert
        assert_eq!(
            value,
            json!({"type":"deny","session_id":"s-1","request_id":"r-1","tool":"Bash"})
        );
    }

    #[test]
    fn stream_delta_is_the_event_that_carries_text() {
        // Arrange
        let raw = json!({"type":"stream_delta","session_id":"s-1","turn_id":"t-1","text":"hi"});

        // Act
        let event: Event = serde_json::from_value(raw).unwrap();

        // Assert
        assert_eq!(
            event,
            Event::StreamDelta {
                session_id: SessionId::new("s-1"),
                turn_id: TurnId::new("t-1"),
                text: "hi".to_owned(),
            }
        );
    }

    #[test]
    fn backend_crashed_defaults_missing_exit_code() {
        // Arrange
        let raw = json!({"type":"backend_crashed","session_id":"s-1"});

        // Act
        let event: Event = serde_json::from_value(raw).unwrap();

        // Assert
        assert_eq!(
            event,
            Event::BackendCrashed {
                session_id: SessionId::new("s-1"),
                exit_code: None,
            }
        );
    }

    #[test]
    fn unknown_event_type_is_rejected() {
        // Arrange
        let raw = json!({"type":"from_the_future","session_id":"s-1"});

        // Act
        let parsed = serde_json::from_value::<Event>(raw);

        // Assert
        assert!(parsed.is_err());
    }
}
