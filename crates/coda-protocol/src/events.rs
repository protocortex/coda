// SPDX-License-Identifier: AGPL-3.0-only

//! Notifications the daemon pushes to subscribed clients.
//!
//! Every variant except [`Event::StreamDelta`] is content-free: it names
//! sessions, turns and tools but never prompts, arguments or outputs.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::{OriginalId, RequestId, SessionId, TokenUsage, TurnId};

/// Notification method that wraps an [`Event`].
pub const EVENT_NOTIFICATION: &str = "session.event";
/// Stored event kind of a permission request that ended without an answer.
pub const TIMEOUT_KIND: &str = "timeout";
/// Stored event kind of an approved permission request.
pub const APPROVE_KIND: &str = "approve";
/// Stored event kind of a denied permission request.
pub const DENY_KIND: &str = "deny";
/// Stored event kind of a finished turn.
pub const RESULT_KIND: &str = "result";
/// Stored event kind of a turn's token usage.
pub const USAGE_KIND: &str = "usage";
/// Stored event kind of the notice that approvals cannot be answered.
pub const APPROVALS_UNAVAILABLE_KIND: &str = "approvals_unavailable";
/// Stored event kind of a piece of streamed model text.
pub const STREAM_DELTA_KIND: &str = "stream_delta";
/// Stored event kind of a backend process that ended unexpectedly.
pub const BACKEND_CRASHED_KIND: &str = "backend_crashed";
/// Stored event kind of a turn that was cut short.
pub const TURN_INTERRUPTED_KIND: &str = "turn_interrupted";
/// Stored event kind of older messages replaced by a summary.
pub const COMPACTION_KIND: &str = "compaction";

/// One event of a session.
///
/// Adding a variant requires a `PROTOCOL_VERSION` bump: older peers reject
/// unknown variants rather than ignoring them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
        /// Whether the daemon starts the backend again for the same turn, so the
        /// turn is still running. Absent means no restart follows.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        will_restart: bool,
        /// The last lines the process wrote to its error stream, when it wrote any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// A running turn was cut short.
    TurnInterrupted {
        /// Session of the turn.
        session_id: SessionId,
        /// The interrupted turn.
        turn_id: TurnId,
    },
    /// The oldest messages of a conversation were replaced by a summary of them.
    /// The summary text is kept in the session's originals, never in the event.
    Compaction {
        /// Session of the turn.
        session_id: SessionId,
        /// The turn that compacted.
        turn_id: TurnId,
        /// Where the summary text is kept.
        summary: OriginalId,
        /// How many of the oldest messages the summary replaced.
        replaced_messages: u32,
        /// How many recent messages were kept as they were.
        kept_messages: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RequestId, SessionId, TurnId};
    use serde_json::json;

    #[test]
    fn the_kind_constants_name_the_variants_they_stand_for() {
        // Arrange
        let kinds = [
            TIMEOUT_KIND,
            APPROVE_KIND,
            DENY_KIND,
            RESULT_KIND,
            USAGE_KIND,
            APPROVALS_UNAVAILABLE_KIND,
            STREAM_DELTA_KIND,
            BACKEND_CRASHED_KIND,
            TURN_INTERRUPTED_KIND,
            COMPACTION_KIND,
        ];

        for kind in kinds {
            // Act
            let fields = json!({
                "type": kind, "session_id": "s", "request_id": "r", "tool": "t",
                "turn_id": "t", "is_error": false, "reason": "x", "text": "",
                "usage": {"input_tokens": 0, "output_tokens": 0},
                "summary": "1", "replaced_messages": 0, "kept_messages": 0,
            });
            let event: Event = serde_json::from_value(fields).unwrap();

            // Assert
            assert_eq!(serde_json::to_value(&event).unwrap()["type"], kind);
        }
    }

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
    fn compaction_carries_a_reference_and_counts_but_no_text() {
        // Arrange
        let event = Event::Compaction {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            summary: OriginalId::new("7"),
            replaced_messages: 12,
            kept_messages: 5,
        };

        // Act
        let value = serde_json::to_value(&event).unwrap();

        // Assert
        assert_eq!(
            value,
            json!({"type":"compaction","session_id":"s-1","turn_id":"t-1",
                   "summary":"7","replaced_messages":12,"kept_messages":5})
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
                will_restart: false,
                detail: None,
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
