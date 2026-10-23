// SPDX-License-Identifier: AGPL-3.0-only

//! Rebuilds a provider-neutral [`Conversation`] from the events a session
//! stored, so a new loop continues where the last one stopped.
//!
//! Replay is full: nothing a session stored is dropped. A send that stopped
//! early is closed the way the live loop closes it, so the history never
//! holds a tool call without a result.

use coda_protocol::events::{
    APPROVALS_UNAVAILABLE_KIND, APPROVE_KIND, BACKEND_CRASHED_KIND, COMPACTION_KIND, DENY_KIND,
    HISTORY_NOT_RESTORED_KIND, HOOK_BLOCKED_KIND, RESULT_KIND, STREAM_DELTA_KIND, TIMEOUT_KIND,
    TURN_INTERRUPTED_KIND, USAGE_KIND,
};
use coda_protocol::types::{OriginalId, SessionId};
use coda_store::{Store, StoreError, StoredEvent};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

mod transcript;

pub use self::transcript::Transcript;

use crate::compact::{SUMMARY_UNAVAILABLE, summary_message};
use crate::prompt::{Conversation, Message, Prefix, ToolCall};
use crate::turn::{EMPTY_ARGUMENTS, NO_REPLY, SKIPPED};

/// Stored kind of what the user said.
pub const USER_MESSAGE_KIND: &str = "user_message";
/// Stored kind of one model reply, with the tool calls it made. The text repeats
/// the stream deltas: events are append-only, so the reply is written whole once
/// it is complete. Call arguments are kept in the session's originals, not here.
pub const ASSISTANT_MESSAGE_KIND: &str = "assistant_message";
/// Stored kind of one finished tool call. The result the model saw is kept in
/// the session's originals, never in the event, so tool output stays out of the log.
pub const TOOL_FINISHED_KIND: &str = "tool_finished";
/// Kind older daemons stored when they dropped a conversation. Replay ignores
/// it: the history before it is still the history.
pub const CONVERSATION_RESET_KIND: &str = "conversation_reset";
/// Stored kind of a permission request that was announced.
pub const PERMISSION_REQUEST_KIND: &str = "permission_request";
/// Stored kind of a context window overflow.
pub const CONTEXT_OVERFLOW_KIND: &str = "context_overflow";
/// Stored kind of a passed `session_start` hook, so a loop built later for the
/// session tells the hook it is resumed. The body is `{}`.
pub const SESSION_STARTED_KIND: &str = "session_started";
/// Stored kind of the reason a send stopped.
pub const TURN_STOPPED_KIND: &str = "turn_stopped";
/// Stored kind of a model switch.
pub const MODEL_SWITCHED_KIND: &str = "model_switched";

/// Stored kind of one event of a sub-agent's run. It wraps the event the child
/// loop produced under the agent's name, so the parent's history never mixes
/// with the child's: replay skips it, and the parent sees the `task` call and
/// its result only.
pub const CHILD_EVENT_KIND: &str = "child_event";

/// Kinds the daemon stores for its own bookkeeping; the model never saw them.
const BOOKKEEPING_KINDS: &[&str] = &[
    APPROVALS_UNAVAILABLE_KIND,
    APPROVE_KIND,
    BACKEND_CRASHED_KIND,
    CHILD_EVENT_KIND,
    CONTEXT_OVERFLOW_KIND,
    DENY_KIND,
    HISTORY_NOT_RESTORED_KIND,
    HOOK_BLOCKED_KIND,
    MODEL_SWITCHED_KIND,
    PERMISSION_REQUEST_KIND,
    RESULT_KIND,
    STREAM_DELTA_KIND,
    TIMEOUT_KIND,
    TURN_INTERRUPTED_KIND,
    TURN_STOPPED_KIND,
    USAGE_KIND,
];
/// Told to the model when a stored tool result can no longer be read.
const RESULT_UNAVAILABLE: &str = "[the output of this call is no longer available]";
/// Events read from the store per query.
const PAGE_SIZE: u32 = 500;
/// Stands for empty text, which the store cannot hold as an original.
const EMPTY_TEXT_ID: &str = "";

#[derive(Deserialize)]
struct UserBody {
    text: String,
}

#[derive(Deserialize)]
struct CompactionBody {
    /// Where the summary text was kept.
    summary: String,
    replaced_messages: usize,
}

#[derive(Deserialize)]
struct AssistantBody {
    text: String,
    #[serde(default)]
    tool_calls: Vec<CallBody>,
}

#[derive(Deserialize)]
struct CallBody {
    id: String,
    name: String,
    /// Where the arguments were kept.
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ToolFinishedBody {
    /// Absent in the bodies older daemons stored, which cannot be matched to a call.
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    original: Option<String>,
    is_error: bool,
}

/// The body stored for a model reply. Each call comes with the id its
/// arguments were kept under, when they could be kept.
#[must_use]
pub fn assistant_body(text: &str, calls: &[(&ToolCall, Option<OriginalId>)]) -> Value {
    let calls: Vec<Value> = calls
        .iter()
        .map(|(call, arguments)| {
            json!({
                "id": call.id,
                "name": call.name,
                "arguments": arguments.as_ref().map(OriginalId::as_str),
            })
        })
        .collect();
    json!({ "text": text, "tool_calls": calls })
}

/// The body stored for a finished tool call. `original` names where the
/// result text was kept, when it could be kept.
#[must_use]
pub fn tool_finished_body(
    tool: &str,
    call_id: &str,
    original: Option<&OriginalId>,
    is_error: bool,
) -> Value {
    json!({
        "tool": tool,
        "call_id": call_id,
        "original": original.map(OriginalId::as_str),
        "is_error": is_error,
    })
}

/// The id that stands for empty text, which the store cannot keep.
#[must_use]
pub fn empty_text_id() -> OriginalId {
    OriginalId::new(EMPTY_TEXT_ID)
}

/// Looks up the stored text of a tool result by the id its event names.
pub type ResultLookup<'a> = &'a dyn Fn(&OriginalId) -> Option<String>;

/// Rebuilds a conversation from `events`, oldest first. Texts come from
/// `lookup`; one it cannot find is replaced by a note or an empty value.
#[must_use]
pub fn rebuild(prefix: Prefix, events: &[StoredEvent], lookup: ResultLookup<'_>) -> Conversation {
    let mut replay = Rebuild::new(prefix);
    for event in events {
        replay.apply(event, lookup);
    }
    replay.finish()
}

/// A conversation being rebuilt from events fed in order, so a caller can read
/// the store a page at a time without holding it for the whole replay.
pub struct Rebuild {
    conversation: Conversation,
    /// Calls of the last assistant message that have no result yet.
    unanswered: Vec<String>,
    /// Whether a stored event says a `session_start` hook passed.
    session_started: bool,
}

impl Rebuild {
    /// Starts an empty conversation behind `prefix`.
    #[must_use]
    pub fn new(prefix: Prefix) -> Self {
        Self {
            conversation: Conversation::new(prefix),
            unanswered: Vec::new(),
            session_started: false,
        }
    }

    /// Whether the events applied so far include a passed `session_start` hook.
    #[must_use]
    pub fn session_started(&self) -> bool {
        self.session_started
    }

    /// Applies the next page of `session`'s events after `after` and returns
    /// the sequence number to continue from, or `None` at the end.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the session or its events cannot be read.
    pub fn apply_page(
        &mut self,
        store: &Store,
        session: &SessionId,
        after: u64,
    ) -> Result<Option<u64>, StoreError> {
        let lookup = |id: &OriginalId| store.load_original(session, id).ok().flatten();
        let page = store.load_events(session, after, PAGE_SIZE)?;
        for event in &page {
            self.apply(event, &lookup);
        }
        Ok(next_after(&page))
    }

    /// Ends the last send and returns the conversation.
    #[must_use]
    pub fn finish(mut self) -> Conversation {
        self.settle();
        self.conversation
    }

    /// Applies one event.
    pub fn apply(&mut self, event: &StoredEvent, lookup: ResultLookup<'_>) {
        match event.kind.as_str() {
            USER_MESSAGE_KIND => {
                if let Some(body) = parse::<UserBody>(event) {
                    self.settle();
                    self.conversation.push(Message::User(body.text));
                }
            }
            ASSISTANT_MESSAGE_KIND => {
                if let Some(body) = parse::<AssistantBody>(event) {
                    self.assistant(body, lookup);
                }
            }
            TOOL_FINISHED_KIND => {
                if let Some(body) = parse::<ToolFinishedBody>(event) {
                    self.tool_result(event.seq, body, lookup);
                }
            }
            COMPACTION_KIND => {
                if let Some(body) = parse::<CompactionBody>(event) {
                    self.compaction(event.seq, &body, lookup);
                }
            }
            CONVERSATION_RESET_KIND => {}
            SESSION_STARTED_KIND => self.session_started = true,
            kind if BOOKKEEPING_KINDS.contains(&kind) => {
                debug!(seq = event.seq, kind, "skipped a bookkeeping event");
            }
            kind => warn!(seq = event.seq, kind, "skipped an event of an unknown kind"),
        }
    }

    fn assistant(&mut self, body: AssistantBody, lookup: ResultLookup<'_>) {
        self.answer_unanswered();
        let tool_calls: Vec<ToolCall> = body
            .tool_calls
            .into_iter()
            .map(|call| ToolCall {
                id: call.id,
                name: call.name,
                arguments: call
                    .arguments
                    .and_then(|id| resolve(&id, lookup))
                    .unwrap_or_else(|| EMPTY_ARGUMENTS.to_owned()),
            })
            .collect();
        self.unanswered = tool_calls.iter().map(|call| call.id.clone()).collect();
        self.conversation.push(Message::Assistant {
            text: body.text,
            tool_calls,
        });
    }

    fn tool_result(&mut self, seq: u64, body: ToolFinishedBody, lookup: ResultLookup<'_>) {
        let Some(call_id) = body.call_id else {
            debug!(seq, "skipped a tool result stored without a call id");
            return;
        };
        let Some(at) = self.unanswered.iter().position(|id| *id == call_id) else {
            warn!(seq, %call_id, "skipped a tool result that answers no open call");
            return;
        };
        self.unanswered.remove(at);
        let text = body
            .original
            .and_then(|id| resolve(&id, lookup))
            .unwrap_or_else(|| RESULT_UNAVAILABLE.to_owned());
        self.conversation.push(Message::ToolResult {
            call_id,
            text,
            is_error: body.is_error,
        });
    }

    /// Replaces the oldest messages with the stored summary, as the live loop did.
    fn compaction(&mut self, seq: u64, body: &CompactionBody, lookup: ResultLookup<'_>) {
        let summary =
            resolve(&body.summary, lookup).unwrap_or_else(|| SUMMARY_UNAVAILABLE.to_owned());
        let message = Message::User(summary_message(&summary));
        if !self
            .conversation
            .replace_prefix(body.replaced_messages, message)
        {
            warn!(
                seq,
                "skipped a compaction that replaces more messages than exist"
            );
        }
    }

    /// Answers the calls that never got a result, as the live loop does for
    /// calls a stopped turn did not run.
    fn answer_unanswered(&mut self) {
        for call_id in std::mem::take(&mut self.unanswered) {
            self.conversation.push(Message::ToolResult {
                call_id,
                text: SKIPPED.to_owned(),
                is_error: true,
            });
        }
    }

    /// Ends the send in progress: every call answered, and a reply last so
    /// roles keep alternating.
    fn settle(&mut self) {
        self.answer_unanswered();
        if matches!(
            self.conversation.messages().last(),
            Some(Message::User(_) | Message::ToolResult { .. })
        ) {
            self.conversation.push(Message::Assistant {
                text: NO_REPLY.to_owned(),
                tool_calls: Vec::new(),
            });
        }
    }
}

fn resolve(id: &str, lookup: ResultLookup<'_>) -> Option<String> {
    if id == EMPTY_TEXT_ID {
        return Some(String::new());
    }
    lookup(&OriginalId::new(id))
}

/// Parses the body of a known kind; a body that does not fit is skipped with a
/// warning that names the event, never its content.
/// Where the next page starts: after the last event of a full page, else nowhere.
fn next_after(page: &[StoredEvent]) -> Option<u64> {
    match page.last() {
        Some(last) if page.len() == PAGE_SIZE as usize => Some(last.seq),
        _ => None,
    }
}

fn parse<T: for<'de> Deserialize<'de>>(event: &StoredEvent) -> Option<T> {
    match serde_json::from_str(&event.body) {
        Ok(body) => Some(body),
        Err(error) => {
            warn!(seq = event.seq, kind = %event.kind, %error, "skipped an event with an unreadable body");
            None
        }
    }
}

#[cfg(test)]
mod tests;
