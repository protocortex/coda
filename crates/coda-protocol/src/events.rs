// SPDX-License-Identifier: Apache-2.0

//! Notifications the daemon pushes to subscribed clients.
//!
//! Every variant except [`Event::StreamDelta`] and [`Event::ToolCall`] is
//! content-free: it names sessions, turns and tools but never prompts, arguments
//! or outputs. A tool call carries a short summary of what it works on, such as
//! a command line or a path, cut to [`MAX_TOOL_SUMMARY_CHARS`].

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
/// Stored event kind of the notice that earlier turns could not be given to a new thread.
pub const HISTORY_NOT_RESTORED_KIND: &str = "history_not_restored";
/// Stored event kind of a piece of streamed model text.
pub const STREAM_DELTA_KIND: &str = "stream_delta";
/// Stored event kind of a backend process that ended unexpectedly.
pub const BACKEND_CRASHED_KIND: &str = "backend_crashed";
/// Stored event kind of a turn that was cut short.
pub const TURN_INTERRUPTED_KIND: &str = "turn_interrupted";
/// Stored event kind of older messages replaced by a summary.
pub const COMPACTION_KIND: &str = "compaction";
/// Stored event kind of a provider's rate-limit report.
pub const RATE_LIMIT_KIND: &str = "rate_limit";
/// Stored event kind of a turn that ended because the plan limit was hit.
pub const USAGE_LIMIT_KIND: &str = "usage_limit";
/// Stored event kind of a tool call that started or finished.
pub const TOOL_CALL_KIND: &str = "tool_call";
/// Longest tool summary, in characters. A longer one is cut and ends in `...`.
pub const MAX_TOOL_SUMMARY_CHARS: usize = 80;
const SUMMARY_ELLIPSIS: &str = "...";
/// Input fields that name what a tool works on, in the order a summary prefers them.
const SUMMARY_FIELDS: [&str; 7] = [
    "command",
    "file_path",
    "pattern",
    "query",
    "url",
    "path",
    "description",
];

/// A short, one-line summary of what a tool call works on, from its JSON input:
/// the first of `command`, `file_path`, `pattern`, `query`, `url`, `path` and
/// `description` that holds text. `None` when none does.
#[must_use]
pub fn tool_summary(input: &serde_json::Value) -> Option<String> {
    SUMMARY_FIELDS
        .iter()
        .find_map(|field| input[field].as_str().and_then(summary_of))
}

/// The first non-blank line of `text` without control characters, cut to
/// [`MAX_TOOL_SUMMARY_CHARS`]. `None` when `text` has no such line.
#[must_use]
pub fn summary_of(text: &str) -> Option<String> {
    let line: String = text
        .lines()
        .map(|line| line.chars().filter(|c| !c.is_control()).collect::<String>())
        .map(|line| line.trim().to_owned())
        .find(|line| !line.is_empty())?;
    if line.chars().count() <= MAX_TOOL_SUMMARY_CHARS {
        return Some(line);
    }
    let keep = MAX_TOOL_SUMMARY_CHARS - SUMMARY_ELLIPSIS.len();
    let mut cut: String = line.chars().take(keep).collect();
    cut.push_str(SUMMARY_ELLIPSIS);
    Some(cut)
}

/// Stored event kind of the reason a blocking `session_start` hook stopped a turn.
/// The body is a [`HookBlockedBody`]; clients show it to the user and never to the model.
pub const HOOK_BLOCKED_KIND: &str = "hook_blocked";

/// The body of a stored `hook_blocked` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookBlockedBody {
    /// Which hook failed and why.
    pub reason: String,
}

/// Stored event kind of an `end_turn` hook sending the model back to work. The
/// body is a [`GuardContinueBody`]. The text is also a user message in the
/// model's history; clients show it as the reason the turn went on.
pub const GUARD_CONTINUE_KIND: &str = "guard_continue";
/// Stored event kind of a turn that ended because an `end_turn` hook still asked
/// for more work after the allowed number of continues. The body is a
/// [`GuardLimitBody`].
pub const GUARD_LIMIT_KIND: &str = "guard_limit";

/// The body of a stored `guard_continue` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardContinueBody {
    /// What the hook said, which the model was told.
    pub text: String,
    /// Which forced continue this is in the send, from 1.
    pub attempt: u32,
}

/// The body of a stored `guard_limit` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardLimitBody {
    /// The last thing the hook said.
    pub text: String,
    /// How many continues were forced.
    pub attempts: u32,
}

/// Stored event kind of a tool call that was repaired or read from reply text.
/// The body is a [`CallRecoveredBody`]; clients show it to the user.
pub const CALL_RECOVERED_KIND: &str = "call_recovered";

/// The `how` of a `call_recovered` event whose arguments were repaired.
pub const CALL_REPAIRED: &str = "repaired";
/// The `how` of a `call_recovered` event whose call was read from reply text.
pub const CALL_FROM_TEXT: &str = "from_text";

/// The body of a stored `call_recovered` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRecoveredBody {
    /// The call, by its id in the conversation.
    pub call_id: String,
    /// The tool the call is for.
    pub tool: String,
    /// What was done: `repaired` (its arguments were fixed) or `from_text` (it
    /// was read from the text of a reply).
    pub how: String,
}

/// One rate-limit window of a provider plan, such as `five_hour` or `seven_day`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RateLimitWindow {
    /// The provider's name for the window.
    pub name: String,
    /// Share of the window used, in basis points: 900 is 9%. It is not capped, so
    /// a window the plan has overrun reports more than 10000.
    pub utilization_bps: u32,
    /// When the window resets, in seconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

/// What a provider reported about its rate limits.
///
/// The status and window names are the provider's own words, since the set can
/// grow without a protocol change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RateLimit {
    /// The provider's status, such as `allowed`.
    pub status: String,
    /// The window the status is about, when the provider names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_type: Option<String>,
    /// When that window resets, in seconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    /// Every window the provider reported.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<RateLimitWindow>,
    /// Whether the plan's usage is being billed as overage.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_using_overage: bool,
    /// The provider's overage status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_status: Option<String>,
    /// When the overage allowance resets, in seconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_resets_at: Option<u64>,
    /// Why overage is unavailable, when the provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_disabled_reason: Option<String>,
}

/// Which plan window a usage limit belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UsageWindowKind {
    /// The rolling five-hour window.
    FiveHour,
    /// The rolling seven-day window.
    SevenDay,
    /// A window that counts one model only, such as the seven-day Opus window.
    ModelScoped,
    /// A monthly window.
    Monthly,
    /// A window this peer does not know, sent by a newer one. Also any name the
    /// provider reports that has no kind of its own, such as `overage`.
    #[serde(other)]
    Unknown,
}

/// Prefix of the provider's names for seven-day windows that count one model.
const MODEL_SCOPED_PREFIX: &str = "seven_day_";

impl UsageWindowKind {
    /// The kind a provider's window name stands for.
    #[must_use]
    pub fn from_window_name(name: &str) -> Self {
        match name {
            "five_hour" => Self::FiveHour,
            "seven_day" => Self::SevenDay,
            "monthly" => Self::Monthly,
            _ if name.starts_with(MODEL_SCOPED_PREFIX) => Self::ModelScoped,
            _ => Self::Unknown,
        }
    }
}

/// Where a tool call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolCallStatus {
    /// The call began and has no result yet.
    Started,
    /// The call finished and the tool reported no error.
    Succeeded,
    /// The call finished with an error, or was denied.
    Failed,
    /// A status this peer does not know, sent by a newer one.
    #[serde(other)]
    Unknown,
}

/// Why a turn was cut short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InterruptCause {
    /// A client cancelled the turn, or the daemon is shutting down.
    Cancelled,
    /// The backend stayed silent past a watchdog limit.
    IdleTimeout,
    /// The backend process went away before the turn could finish.
    BackendLost,
    /// The daemon stopped while the turn ran, and found it open on the next start.
    DaemonRestarted,
    /// The daemon is shutting down and did not start the backend again.
    DaemonStopping,
    /// A cause this peer does not know, sent by a newer one.
    #[serde(other)]
    Unknown,
}

impl InterruptCause {
    /// The name the cause has on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::IdleTimeout => "idle_timeout",
            Self::BackendLost => "backend_lost",
            Self::DaemonRestarted => "daemon_restarted",
            Self::DaemonStopping => "daemon_stopping",
            Self::Unknown => "unknown",
        }
    }
}

/// One event of a session.
///
/// A variant added later reads as [`Event::Unknown`] on an older peer, so
/// adding one does not need a `PROTOCOL_VERSION` bump. The daemon announces
/// the matching `Capabilities` flag at `initialize`.
///
/// `Unknown` is receive-only. It keeps nothing of the original event, so it
/// serializes as `{"type":"unknown"}`. A client or relay must not store or
/// forward it as if it were the original; keep the raw JSON line instead when
/// the original has to survive.
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
    /// A new backend thread could not be given the conversation so far.
    HistoryNotRestored {
        /// Affected session.
        session_id: SessionId,
        /// Why the earlier turns are missing.
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
        /// Why the turn was cut short; absent in events stored before it was recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<InterruptCause>,
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
    /// The provider reported its rate-limit state.
    RateLimit {
        /// Affected session.
        session_id: SessionId,
        /// What the provider reported.
        limit: RateLimit,
    },
    /// The turn ended in error because the provider's plan limit was hit.
    /// Sent just before that turn's `result`.
    UsageLimit {
        /// Affected session.
        session_id: SessionId,
        /// The turn the limit ended.
        turn_id: TurnId,
        /// The window that ran out.
        window: UsageWindowKind,
        /// When the window resets, in seconds since the Unix epoch, when the provider says.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reset_at: Option<u64>,
    },
    /// A tool call started or finished. A call sends one event for each, with the
    /// same `id`, `name` and `summary`. A turn that ends in `turn_interrupted` or
    /// `backend_crashed` first ends its open calls as `failed`. Only the session's
    /// own calls are reported, not those of a sub-agent it started.
    ToolCall {
        /// Session of the turn.
        session_id: SessionId,
        /// The turn that made the call.
        turn_id: TurnId,
        /// The backend's id for the call; a start and its finish share it.
        id: String,
        /// The tool's name as its backend calls it, such as `Bash` or `bash`.
        name: String,
        /// What the call works on, cut to [`MAX_TOOL_SUMMARY_CHARS`], when it has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// Whether the call began, or how it ended.
        status: ToolCallStatus,
    },
    /// An event of a type this peer does not know, sent by a newer one.
    #[serde(other)]
    Unknown,
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
            HISTORY_NOT_RESTORED_KIND,
            STREAM_DELTA_KIND,
            BACKEND_CRASHED_KIND,
            TURN_INTERRUPTED_KIND,
            COMPACTION_KIND,
            RATE_LIMIT_KIND,
            USAGE_LIMIT_KIND,
            TOOL_CALL_KIND,
        ];

        for kind in kinds {
            // Act
            let fields = json!({
                "type": kind, "session_id": "s", "request_id": "r", "tool": "t",
                "turn_id": "t", "is_error": false, "reason": "x", "text": "",
                "usage": {"input_tokens": 0, "output_tokens": 0},
                "summary": "1", "replaced_messages": 0, "kept_messages": 0,
                "limit": {"status": "allowed"}, "window": "five_hour",
                "id": "c", "name": "Bash", "status": "started",
            });
            let event: Event = serde_json::from_value(fields).unwrap();

            // Assert
            assert_eq!(serde_json::to_value(&event).unwrap()["type"], kind);
        }
    }

    #[test]
    fn a_hook_blocked_body_is_written_as_a_reason_and_read_back() {
        // Arrange
        let body = HookBlockedBody {
            reason: "blocked by the session_start hook `gate`".to_owned(),
        };

        // Act
        let written = serde_json::to_value(&body).unwrap();
        let read: HookBlockedBody = serde_json::from_value(written.clone()).unwrap();

        // Assert
        assert_eq!(
            written,
            json!({"reason": "blocked by the session_start hook `gate`"})
        );
        assert_eq!(read, body);
        assert_eq!(HOOK_BLOCKED_KIND, "hook_blocked");
    }

    #[test]
    fn a_cause_names_itself_as_it_is_written_on_the_wire() {
        // Arrange
        let causes = [
            InterruptCause::Cancelled,
            InterruptCause::IdleTimeout,
            InterruptCause::BackendLost,
            InterruptCause::DaemonRestarted,
            InterruptCause::DaemonStopping,
            InterruptCause::Unknown,
        ];

        for cause in causes {
            // Act
            let written = serde_json::to_value(cause).unwrap();

            // Assert
            assert_eq!(written, json!(cause.as_str()));
        }
    }

    #[test]
    fn a_summary_takes_the_first_field_that_names_the_work() {
        let input = json!({"description": "last", "path": "src/a.rs", "command": ""});
        let search = json!({"pattern": "TODO", "path": "src"});
        assert_eq!(tool_summary(&search), Some("TODO".to_owned()));

        assert_eq!(tool_summary(&input), Some("src/a.rs".to_owned()));
        assert_eq!(
            tool_summary(&json!({"command": "cargo test"})),
            Some("cargo test".to_owned())
        );
        assert_eq!(tool_summary(&json!({"other": "x"})), None);
        assert_eq!(tool_summary(&json!("text")), None);
    }

    #[test]
    fn a_summary_keeps_the_first_line_and_cuts_a_long_one() {
        let long = "x".repeat(MAX_TOOL_SUMMARY_CHARS + 20);

        let cut = summary_of(&long).unwrap();

        assert_eq!(cut.chars().count(), MAX_TOOL_SUMMARY_CHARS);
        assert!(cut.ends_with("..."));
        assert_eq!(
            summary_of("\n  ls -la  \nsecond"),
            Some("ls -la".to_owned())
        );
        assert_eq!(summary_of("  \n "), None);
        assert_eq!(summary_of("ls\x1b[31m\r"), Some("ls[31m".to_owned()));
        assert_eq!(
            summary_of(&"é".repeat(MAX_TOOL_SUMMARY_CHARS))
                .unwrap()
                .chars()
                .count(),
            MAX_TOOL_SUMMARY_CHARS
        );
    }

    #[test]
    fn a_window_name_maps_to_its_kind() {
        let cases = [
            ("five_hour", UsageWindowKind::FiveHour),
            ("seven_day", UsageWindowKind::SevenDay),
            ("seven_day_opus", UsageWindowKind::ModelScoped),
            ("seven_day_sonnet", UsageWindowKind::ModelScoped),
            ("monthly", UsageWindowKind::Monthly),
            ("overage", UsageWindowKind::Unknown),
        ];

        for (name, kind) in cases {
            assert_eq!(UsageWindowKind::from_window_name(name), kind, "{name}");
        }
    }

    #[test]
    fn a_window_kind_from_a_newer_peer_reads_as_unknown() {
        let read: UsageWindowKind = serde_json::from_value(json!("yearly")).unwrap();

        assert_eq!(read, UsageWindowKind::Unknown);
        assert_eq!(
            serde_json::to_value(UsageWindowKind::ModelScoped).unwrap(),
            json!("model_scoped")
        );
    }

    #[test]
    fn an_unknown_event_keeps_nothing_of_the_original_so_it_is_receive_only() {
        // Arrange
        let raw = json!({"type":"event_added_later","session_id":"s-1","extra":{"n":1}});

        // Act
        let event: Event = serde_json::from_value(raw.clone()).unwrap();
        let written = serde_json::to_value(&event).unwrap();

        // Assert
        assert_eq!(event, Event::Unknown);
        assert_eq!(written, json!({"type":"unknown"}));
        assert_ne!(written, raw);
    }

    #[test]
    fn an_event_without_a_type_does_not_read_as_unknown() {
        let read = serde_json::from_value::<Event>(json!({"session_id":"s-1"}));

        assert!(read.is_err());
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
}
