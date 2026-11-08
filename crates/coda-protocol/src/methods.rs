// SPDX-License-Identifier: Apache-2.0

//! Method names, parameters and results for requests between client and daemon.
//!
//! Structs tolerate unknown fields so older peers keep working, and optional
//! fields default so newer clients can omit them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::events::RateLimitWindow;
use crate::types::{
    BackendKind, Mode, PermissionDecision, RequestId, SessionId, SessionInfo, TokenUsage, TurnId,
};

/// Longest `client_request_id` the daemon accepts.
pub const MAX_REQUEST_ID_BYTES: usize = 128;

/// Protocol version announced and checked during `initialize`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Handshake that opens a connection.
pub const INITIALIZE: &str = "initialize";
/// Starts a session.
pub const SESSION_CREATE: &str = "session.create";
/// Sends a user message, starting a turn.
pub const SESSION_SEND: &str = "session.send";
/// Lists, resumes or edits the follow-up messages queued behind a running turn.
pub const SESSION_QUEUE: &str = "session.queue";
/// Turns on or off continuing a session when the plan window that stopped it resets.
pub const SESSION_RESUME_AT_RESET: &str = "session.resume_at_reset";
/// Compacts the conversation of an idle Claude Code session.
pub const SESSION_COMPACT: &str = "session.compact";
/// Run a named agent on a prompt in a session of a delegated backend.
pub const AGENT_RUN: &str = "agent.run";
/// Cancels the running turn.
pub const SESSION_CANCEL: &str = "session.cancel";
/// Starts streaming a session's events to this connection.
pub const SESSION_SUBSCRIBE: &str = "session.subscribe";
/// Reopens a stored session.
pub const SESSION_RESUME: &str = "session.resume";
/// Releases what the daemon holds for an idle session; its events stay stored.
pub const SESSION_CLOSE: &str = "session.close";
/// Restores the files and rolls back the conversation to the end of an earlier turn.
pub const SESSION_REWIND: &str = "session.rewind";
/// Starts a new session from a turn of an existing one.
pub const SESSION_FORK: &str = "session.fork";
/// Reports the files one turn changed, and with `patch` the unified diff.
pub const SESSION_DIFF: &str = "session.diff";
/// Moves a session that has not started into a git worktree made for it.
pub const SESSION_WORKTREE: &str = "session.worktree";
/// Commits the changes of a session, and on request pushes the branch and opens a pull request.
pub const SESSION_SHIP: &str = "session.ship";
/// Adds a scheduled prompt.
pub const SCHEDULE_ADD: &str = "schedule.add";
/// Lists the scheduled prompts.
pub const SCHEDULE_LIST: &str = "schedule.list";
/// Removes a scheduled prompt.
pub const SCHEDULE_REMOVE: &str = "schedule.remove";
/// Starts a watch of a pull request that wakes the session on news.
pub const SESSION_WATCH_PR: &str = "session.watch_pr";
/// Reports, and with `apply` removes, the git worktrees coda made that are no longer needed.
pub const WORKTREE_CLEAN: &str = "worktree.clean";
/// Reports, and with `apply` removes, old checkpoint refs.
pub const CHECKPOINTS_PRUNE: &str = "checkpoints.prune";
/// Lists stored sessions, without their content.
pub const SESSION_LIST: &str = "session.list";
/// Answers a pending permission request.
pub const PERMISSION_RESPOND: &str = "permission.respond";
/// Adds a standing permission grant.
pub const GRANT_ADD: &str = "grant.add";
/// Reports token usage.
pub const USAGE_SUMMARY: &str = "usage.summary";
/// Changes the autonomy mode of an idle session.
pub const SESSION_SET_MODE: &str = "session.set_mode";
/// Switches the model of a session.
pub const SESSION_SET_MODEL: &str = "session.set_model";
/// Lists the backends a session can start on.
pub const DAEMON_BACKENDS: &str = "daemon.backends";
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
    SESSION_QUEUE,
    SESSION_RESUME_AT_RESET,
    SESSION_COMPACT,
    AGENT_RUN,
    SESSION_CANCEL,
    SESSION_SUBSCRIBE,
    SESSION_RESUME,
    SESSION_LIST,
    SESSION_CLOSE,
    SESSION_REWIND,
    SESSION_FORK,
    SESSION_DIFF,
    SESSION_WORKTREE,
    SESSION_SHIP,
    SESSION_WATCH_PR,
    SCHEDULE_ADD,
    SCHEDULE_LIST,
    SCHEDULE_REMOVE,
    WORKTREE_CLEAN,
    CHECKPOINTS_PRUNE,
    PERMISSION_RESPOND,
    GRANT_ADD,
    USAGE_SUMMARY,
    SESSION_SET_MODE,
    SESSION_SET_MODEL,
    DAEMON_BACKENDS,
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

/// Optional features a daemon offers. A flag the peer does not send reads as
/// false, so a client can tell an older daemon from one that has the feature.
///
/// Add a flag when a change adds events or fields and is not breaking. Bump
/// [`PROTOCOL_VERSION`] only for a breaking change.
///
/// A flag is daemon-wide and says the daemon can send the event or field. It
/// does not say every session does: today only Claude Code sessions produce
/// `usage`, `rate_limit` and `usage_limit` details, so a session on another
/// backend never sends them even when the flag is true. A client must not wait
/// for one.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is its own capability on the wire"
)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Capabilities {
    /// The daemon can report the full usage of a turn, including cache tokens.
    #[serde(default)]
    pub usage_details: bool,
    /// The daemon can send `rate_limit` events. Claude Code and Codex app-server sessions do.
    #[serde(default)]
    pub rate_limit_events: bool,
    /// The daemon can send a `usage_limit` event when a turn ends at the plan limit.
    /// Claude Code and Codex app-server sessions do.
    #[serde(default)]
    pub usage_limit_events: bool,
    /// The daemon can send an `account` event that says how a session is billed.
    /// Claude Code and Codex app-server sessions do.
    #[serde(default)]
    pub account_events: bool,
    /// The daemon can send `tool_call` events. Claude Code, Codex and coda's own
    /// loop do; a backend without tool calls never does.
    #[serde(default)]
    pub tool_call_events: bool,
    /// The daemon honors `force` on `session.cancel`.
    #[serde(default)]
    pub force_cancel: bool,
    /// The daemon serves `session.rewind`.
    #[serde(default)]
    pub rewind: bool,
    /// The daemon serves `session.set_mode`.
    #[serde(default)]
    pub set_mode: bool,
    /// The daemon serves `session.diff`.
    #[serde(default)]
    pub diff: bool,
    /// The daemon serves `mode: queue` on `session.send` and `session.queue`.
    #[serde(default)]
    pub queued_sends: bool,
    /// The daemon serves `mode: steer` on `session.send`.
    #[serde(default)]
    pub steer: bool,
    /// The daemon honors `client_request_id` on `session.create`, `session.send`
    /// and `session.cancel`.
    #[serde(default)]
    pub idempotent_commands: bool,
    /// The daemon serves `session.resume_at_reset`.
    #[serde(default)]
    pub resume_at_reset: bool,
    /// The daemon serves `session.fork` for sessions on Claude Code and Codex.
    #[serde(default)]
    pub fork: bool,
    /// The daemon serves `session.compact`, for Claude Code sessions.
    #[serde(default)]
    pub compact: bool,
}

/// Result of [`INITIALIZE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InitializeResult {
    /// Protocol version the daemon speaks.
    pub protocol_version: u32,
    /// Version of the daemon build.
    pub daemon_version: String,
    /// Optional features the daemon offers; absent from daemons that predate them.
    #[serde(default)]
    pub capabilities: Capabilities,
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
    /// Names this request, so a retry returns the first answer instead of making
    /// another session. At most [`MAX_REQUEST_ID_BYTES`] bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_request_id: Option<String>,
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
    /// Most agent turns the session's loop may take, when it is built for this send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Most tokens the session's loop may spend, when it is built for this send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_cap: Option<u64>,
    /// What to do when the session already has a turn running.
    #[serde(default, skip_serializing_if = "SendMode::is_start")]
    pub mode: SendMode,
    /// Names this request, so a retry returns the first answer instead of
    /// sending the message again. At most [`MAX_REQUEST_ID_BYTES`] bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_request_id: Option<String>,
}

/// What [`SESSION_SEND`] does with a message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
    /// Start a turn now. A session with a turn running refuses.
    #[default]
    Start,
    /// Start a turn now when the session is free and nothing waits; otherwise keep
    /// the message and start it when the running turn ends cleanly. A turn that
    /// was stopped, failed or hit a limit leaves the queue waiting until the
    /// client resumes it with [`SESSION_QUEUE`] or sends a new message that
    /// ends cleanly. A message queued with limits keeps them.
    Queue,
    /// Add the message to the turn that is running, so the agent reads it before
    /// it goes on. Refused when no turn is running or the backend cannot take
    /// one. A message that reaches a turn as it ends may not be read.
    Steer,
}

impl SendMode {
    #[must_use]
    pub fn is_start(&self) -> bool {
        *self == Self::Start
    }
}

/// Result of [`SESSION_SEND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSendResult {
    /// The turn the message started, or for a queued message the latest turn of
    /// the session, which it waits behind.
    pub turn_id: TurnId,
    /// The message was queued and has not started.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub queued: bool,
    /// The message was added to the running turn, which `turn_id` names.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub steered: bool,
}

/// Parameters of [`SESSION_COMPACT`]. The daemon refuses while a turn runs, for a
/// session that is not on Claude Code, and for one with no conversation yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCompactParams {
    /// Target session.
    pub session_id: SessionId,
}

/// Result of [`SESSION_COMPACT`]; the answer comes when the compaction is done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCompactResult {}

/// Parameters of [`SESSION_RESUME_AT_RESET`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionResumeAtResetParams {
    /// Target session.
    pub session_id: SessionId,
    /// Whether the daemon continues the session when the plan window that stopped
    /// it resets. The setting is kept in memory, so a daemon restart clears it, and
    /// it ends a wait that is already running when turned off.
    pub enabled: bool,
}

/// Result of [`SESSION_RESUME_AT_RESET`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionResumeAtResetResult {
    /// The setting after the change.
    pub enabled: bool,
}

/// Parameters of [`SESSION_QUEUE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionQueueParams {
    /// Target session.
    pub session_id: SessionId,
    /// What to do with the queue.
    #[serde(flatten)]
    pub action: QueueAction,
}

/// What [`SESSION_QUEUE`] does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum QueueAction {
    /// Report the waiting messages.
    List,
    /// Start the oldest waiting message when the session is free. Each clean
    /// turn end then starts the next one.
    Resume,
    /// Remove one waiting message, for a client that edits or drops it.
    Discard {
        /// The message to remove.
        queue_id: i64,
    },
}

/// A message waiting for its turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueuedMessage {
    /// Identifies the message to [`QueueAction::Discard`].
    pub id: i64,
    pub text: String,
    /// When it was queued, in milliseconds since the Unix epoch.
    pub at: i64,
}

/// Result of [`SESSION_QUEUE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionQueueResult {
    /// The messages still waiting, oldest first.
    pub messages: Vec<QueuedMessage>,
    /// The turn a resume started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<TurnId>,
}

/// Parameters of [`AGENT_RUN`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentRunParams {
    /// Target session. It must use Claude Code or Codex and be in plan mode,
    /// where the daemon denies every permission request, bans the editing and
    /// shell tools of Claude Code and keeps Codex read-only.
    pub session_id: SessionId,
    /// Name of the agent definition to run, for example `reviewer`.
    pub agent: String,
    /// The request the agent is given, after its own instructions.
    pub text: String,
}

/// Result of [`AGENT_RUN`]: the turn it started, as for [`SESSION_SEND`].
pub type AgentRunResult = SessionSendResult;

/// Parameters of [`SESSION_CANCEL`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCancelParams {
    /// Target session.
    pub session_id: SessionId,
    /// Names this request, so a retry returns the first answer instead of
    /// cancelling again, which could stop a later turn. At most
    /// [`MAX_REQUEST_ID_BYTES`] bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_request_id: Option<String>,
    /// Also kill the backend's process group at once, for a stop that did not
    /// take. Only a daemon that reports `force_cancel` honors it; an older one
    /// ignores it and cancels as usual.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

/// Result of [`SESSION_CANCEL`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCancelResult {
    /// Whether a running turn was cancelled.
    pub cancelled: bool,
}

/// Error code of a request refused because the session has a turn running.
pub const TURN_RUNNING_CODE: i64 = -32003;

/// Parameters of [`SESSION_CLOSE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCloseParams {
    /// Session to release.
    pub session_id: SessionId,
}

/// Result of [`SESSION_CLOSE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCloseResult {
    /// Whether the session held a backend server or conversation that was released.
    pub closed: bool,
}

/// Error code of a [`SESSION_REWIND`] the daemon refused, with the reason as the
/// message: no snapshot for that turn, another running session in the same
/// folder, a held `index.lock`, or a folder that is not a git repository.
pub const REWIND_REFUSED_CODE: i64 = -32007;

/// Parameters of [`SESSION_REWIND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionRewindParams {
    /// Session to rewind. It must have no turn running.
    pub session_id: SessionId,
    /// The turn to go back to: the files return to how this turn left them and
    /// the turns after it are rolled back. Omit it to go back to before the
    /// first turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    /// Leave the working tree as it is and roll back the conversation only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_files: bool,
    /// Restore the files only and keep every turn in the conversation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_conversation: bool,
}

/// Result of [`SESSION_REWIND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionRewindResult {
    /// Whether the working tree was restored.
    pub files_restored: bool,
    /// How many turns were newly rolled back. They stay stored and shown, and the
    /// conversation the backend is given leaves them out.
    pub turns_rolled_back: u64,
    /// The private git ref that holds the files as they were just before the
    /// restore, so work made since the last turn can be brought back with
    /// `git restore --source <ref>`. Absent when the files were not restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_as: Option<String>,
}

/// Parameters of [`SESSION_FORK`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionForkParams {
    /// Session to fork. Its backend must be Claude Code or Codex, and it must have
    /// no turn running.
    pub session_id: SessionId,
    /// The last turn the fork keeps. Omit it for the latest turn that is not
    /// rolled back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
}

/// Result of [`SESSION_FORK`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionForkResult {
    /// The new session. It has the backend, model, folder and mode of the parent,
    /// shares its folder, and starts its backend conversation on its first send.
    pub session: SessionInfo,
    /// The session that was forked.
    pub parent_session_id: SessionId,
    /// The last turn of the parent that the fork keeps.
    pub source_turn: TurnId,
}

/// Error code of a [`SESSION_DIFF`] the daemon refused, with the reason as the
/// message: no snapshot of that turn, or a folder that is not a git repository.
pub const DIFF_REFUSED_CODE: i64 = -32010;

/// Parameters of [`SESSION_DIFF`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionDiffParams {
    /// Session to read. It must have no turn running.
    pub session_id: SessionId,
    /// The turn to report. Omit it for the latest turn that has a snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    /// Also return the unified diff, not only the list of files.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub patch: bool,
}

/// One file a turn changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DiffFile {
    /// Path from the top of the repository.
    pub path: String,
    /// Lines added; zero for a binary file.
    pub added: u64,
    /// Lines removed; zero for a binary file.
    pub deleted: u64,
    /// Whether git sees the file as binary and counts no lines.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
}

/// Result of [`SESSION_DIFF`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionDiffResult {
    /// The turn the files belong to.
    pub turn_id: TurnId,
    /// The files the turn changed itself. Files that only came in with a pull,
    /// merge or rebase during the turn are left out. At most one thousand are
    /// listed.
    pub files: Vec<DiffFile>,
    /// The unified diff of those files; empty unless `patch` was asked for.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub patch: String,
    /// Whether the list of files or the diff was cut for size.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Error code of a [`SESSION_WORKTREE`] the daemon refused, with the reason as
/// the message: the session already has a worktree or a turn, its folder is not
/// a git repository, or the worktree could not be made.
pub const WORKTREE_REFUSED_CODE: i64 = -32008;

/// How long a client should wait for [`SESSION_WORKTREE`]: the daemon runs the
/// user's setup command, which it stops after ten minutes, before it answers.
pub const WORKTREE_REQUEST_TIMEOUT_SECS: u64 = 660;

/// Parameters of [`SESSION_WORKTREE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionWorktreeParams {
    /// Session to move. It must have no turn yet and no worktree.
    pub session_id: SessionId,
}

/// Result of [`SESSION_WORKTREE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionWorktreeResult {
    /// The session, whose working directory is now the worktree.
    pub session: SessionInfo,
    /// The branch coda made for the worktree.
    pub branch: String,
}

/// Error code of a [`SESSION_SHIP`] the daemon refused or that a git or `gh`
/// step failed, with the reason as the message. Steps that ran before the
/// failure are not undone.
pub const SHIP_REFUSED_CODE: i64 = -32011;

/// How far [`SESSION_SHIP`] goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShipAction {
    /// Commit only.
    Commit,
    /// Commit, then push the branch to `origin`.
    CommitPush,
    /// Commit, push, then open a draft pull request with `gh`.
    CommitPushPr,
}

/// How long a client should wait for [`SESSION_SHIP`]: commit hooks, a push and
/// `gh` each have their own limit inside the daemon.
pub const SHIP_REQUEST_TIMEOUT_SECS: u64 = 900;

/// Parameters of [`SESSION_SHIP`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionShipParams {
    /// The session whose working directory is shipped.
    pub session_id: SessionId,
    /// How far to go.
    pub action: ShipAction,
    /// The commit message. Its first line is also the pull request title.
    pub message: String,
    /// The pull request body, which should say why the change is made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Open the pull request ready for review instead of as a draft.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ready: bool,
    /// Stage every change first. Off by default, so only what is already staged is committed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
}

/// Result of [`SESSION_SHIP`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionShipResult {
    /// The new commit.
    pub commit: String,
    /// The branch that was committed to.
    pub branch: String,
    /// The files the commit holds, as paths relative to the repository root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Whether the branch was pushed.
    pub pushed: bool,
    /// The address of the pull request that was opened, when the action asked for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<String>,
}

/// Error code of a [`SESSION_WATCH_PR`] the daemon refused, with the reason as the message.
pub const WATCH_REFUSED_CODE: i64 = -32012;

/// Parameters of [`SESSION_WATCH_PR`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionWatchPrParams {
    /// The session to wake. The pull request is read in its working directory.
    pub session_id: SessionId,
    /// The pull request, as a number or an https web address.
    pub pr: String,
}

/// Result of [`SESSION_WATCH_PR`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionWatchPrResult {
    /// Every pull request the session watches now, the new one included.
    pub watching: Vec<String>,
}

/// Error code of a [`SCHEDULE_ADD`] the daemon refused, with the reason as the message.
pub const SCHEDULE_REFUSED_CODE: i64 = -32013;

/// The fewest seconds between two runs of an interval schedule.
pub const MIN_SCHEDULE_INTERVAL_SECS: u64 = 60;

/// Most scheduled prompts the daemon keeps.
pub const MAX_SCHEDULES: usize = 50;

/// A day of the week.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    /// Every day, Monday first.
    pub const ALL: [Weekday; 7] = [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
        Weekday::Sat,
        Weekday::Sun,
    ];

    /// Position in the week, Monday as 0.
    #[must_use]
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|day| *day == self).unwrap_or(0)
    }
}

/// When a scheduled prompt runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleSpec {
    /// Once, at a time given in milliseconds since the Unix epoch.
    Once { at: i64 },
    /// Again and again, this many seconds apart; at least [`MIN_SCHEDULE_INTERVAL_SECS`].
    Interval { every_secs: u64 },
    /// At a time of day in UTC, on the listed weekdays, or every day when none is listed.
    Daily {
        /// Minutes after midnight UTC, from 0 to 1439.
        minute_of_day: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        weekdays: Vec<Weekday>,
    },
}

/// Where a scheduled prompt goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleTarget {
    /// Into a session that exists.
    Session { session_id: SessionId },
    /// Into a new session made for each run.
    NewSession {
        backend: BackendKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Absolute path of the working directory.
        cwd: String,
        #[serde(default)]
        mode: Mode,
    },
}

/// Where a scheduled prompt is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleState {
    /// Waiting for its next run.
    Idle,
    /// A run has started and has not finished.
    Running,
    /// A one-time prompt that has run.
    Done,
}

/// Parameters of [`SCHEDULE_ADD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScheduleAddParams {
    pub spec: ScheduleSpec,
    pub target: ScheduleTarget,
    /// The prompt sent at each run.
    pub text: String,
}

/// One scheduled prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScheduleInfo {
    pub id: u64,
    pub spec: ScheduleSpec,
    pub target: ScheduleTarget,
    pub text: String,
    pub state: ScheduleState,
    /// When it runs next, in milliseconds since the Unix epoch; none once a one-time prompt ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<i64>,
    /// When it last started, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<i64>,
    /// How many runs have started.
    pub runs: u64,
    pub created_at: i64,
}

/// Result of [`SCHEDULE_ADD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScheduleAddResult {
    pub schedule: ScheduleInfo,
}

/// Parameters of [`SCHEDULE_LIST`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScheduleListParams {}

/// Result of [`SCHEDULE_LIST`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScheduleListResult {
    pub schedules: Vec<ScheduleInfo>,
}

/// Parameters of [`SCHEDULE_REMOVE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScheduleRemoveParams {
    pub id: u64,
}

/// Result of [`SCHEDULE_REMOVE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScheduleRemoveResult {
    /// Whether a scheduled prompt with that id existed.
    pub removed: bool,
}

/// Parameters of [`WORKTREE_CLEAN`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeCleanParams {
    /// Remove what the report lists for removal. Without it nothing changes and
    /// the result only says what would go. The daemon refuses it while
    /// `[worktree.cleanup] enabled` is off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub apply: bool,
}

/// What cleanup decided about one worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CleanVerdict {
    /// The worktree can be removed.
    Remove,
    /// The worktree stays; the reason says why.
    Keep,
    /// The folder is already gone; nothing to do.
    Gone,
}

/// One worktree coda made, as cleanup saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CleanWorktree {
    /// The session the worktree belongs to.
    pub session_id: SessionId,
    /// The worktree folder.
    pub path: String,
    /// The branch coda made for it.
    pub branch: String,
    /// Bytes the folder holds, counting what `.gitignore` excludes.
    pub bytes: u64,
    /// What cleanup decided.
    pub verdict: CleanVerdict,
    /// Why, in words.
    pub reason: String,
    /// Whether this call removed it.
    pub removed: bool,
    /// Why a removal that was tried did not happen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A folder in the managed folder that no session owns. It is never removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UnknownFolder {
    /// The folder.
    pub path: String,
    /// Bytes it holds.
    pub bytes: u64,
}

/// Result of [`WORKTREE_CLEAN`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeCleanResult {
    /// Whether removals were allowed and tried in this call.
    pub applied: bool,
    /// Every worktree coda made.
    pub worktrees: Vec<CleanWorktree>,
    /// Folders in the managed folder that coda did not make.
    pub unknown: Vec<UnknownFolder>,
}

/// Error code of a [`CHECKPOINTS_PRUNE`] with `apply` that the daemon refused
/// because pruning is off in its config.
pub const PRUNE_REFUSED_CODE: i64 = -32009;

/// Parameters of [`CHECKPOINTS_PRUNE`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckpointsPruneParams {
    /// Remove what the report lists. Without it nothing changes and the result
    /// only says what would go. The daemon refuses it while
    /// `[checkpoints.prune] enabled` is off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub apply: bool,
}

/// One checkpoint ref that pruning removes or removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrunedRef {
    /// The git ref.
    pub reference: String,
    /// Why it can go.
    pub reason: String,
    /// Whether this call removed it.
    pub removed: bool,
}

/// What pruning found for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrunedSession {
    /// The session.
    pub session_id: SessionId,
    /// How many of its checkpoint refs stay.
    pub kept: u64,
    /// The refs that go.
    pub refs: Vec<PrunedRef>,
    /// An estimate of the bytes that `git gc` can free once these refs are
    /// gone: what only these refs keep alive. Zero when it could not be worked out.
    pub bytes: u64,
    /// Why the session was left alone, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Result of [`CHECKPOINTS_PRUNE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckpointsPruneResult {
    /// Whether removals were allowed and tried in this call.
    pub applied: bool,
    /// Every session that has checkpoint refs.
    pub sessions: Vec<PrunedSession>,
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
    /// Time of the session's last event, or its creation time when it has none, in
    /// milliseconds since the Unix epoch. Absent from daemons that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
}

/// Parameters of [`SESSION_LIST`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionListParams {
    /// Only sessions started in this working directory, or every session when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Most sessions to return; the daemon applies its own default and ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Whether a session has a turn running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionStatus {
    /// No turn is running.
    Idle,
    /// A turn is starting or running.
    Running,
}

/// What a session waits for from a person.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionAttention {
    /// Nothing waits for an answer.
    #[default]
    None,
    /// A tool call waits for permission.
    Approval,
    /// The model asked a question and waits for the answer.
    Input,
    /// A state a newer daemon reports that this client does not know.
    #[serde(other)]
    Unknown,
}

/// Metadata of one stored session; never its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSummary {
    /// Session identifier.
    pub id: SessionId,
    /// Backend driving the session.
    pub backend: BackendKind,
    /// Working directory of the session.
    pub cwd: String,
    /// Creation time, in milliseconds since the Unix epoch.
    pub created_at: i64,
    /// Time of the last event, or the creation time when there is none.
    pub updated_at: i64,
    /// Whether a turn is running.
    pub status: SessionStatus,
    /// What the session waits for from a person. A question outranks an approval.
    /// Absent from daemons that predate it.
    #[serde(default)]
    pub attention: SessionAttention,
    /// The start of the first message the user sent, when there is one; a session
    /// that has not started has none. Absent from daemons that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Pull requests the session opened, newest first. Absent from daemons that predate it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pull_requests: Vec<String>,
}

/// Result of [`SESSION_LIST`], newest session first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionListResult {
    /// The matching sessions.
    pub sessions: Vec<SessionSummary>,
}

/// Params of the `permission.request` notification that asks a client to
/// answer a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionRequestParams {
    /// Session that raised the request.
    pub session_id: SessionId,
    /// The request to answer with [`PERMISSION_RESPOND`].
    pub request_id: RequestId,
    /// Normalized name of the tool that asks.
    pub tool: String,
    /// Canonical root of the git repository the call runs in.
    pub repo_path: String,
    /// The command line, or the tool name followed by the paths it acts on
    /// when the call has no command line.
    pub command: String,
    /// Name of the sub-agent that asks, absent when the session's own call
    /// asks. A client shows it apart from `command`, which is the model's text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// For a question the model asks the user (`tool` is `ask`): the choices,
    /// in the order the answer's `choice` counts them from 0. `command` is then
    /// the question. Empty for a tool call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// What the backend or its model says the call is for, cut to 500
    /// characters. Absent when it gave nothing, as coda's own loop does. A
    /// client shows it apart from `command` and as a claim, since the text is
    /// not checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
    /// For a question the model asked: the option picked, counted from 0. Absent
    /// when the user declined to pick one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<u32>,
    /// True when the client has no user to ask, as `coda run` has none. The
    /// model is told so, instead of being told the user declined.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_terminal: bool,
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
    /// The plan limit windows the provider last reported, whichever session saw them. A
    /// window the daemon has no report for is missing; one past its reset time may still be
    /// listed, so a client checks `resets_at`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<RateLimitWindow>,
}

/// Parameters of [`SESSION_SET_MODE`]. The daemon refuses the change while a
/// turn runs and tells no other subscriber of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSetModeParams {
    /// Target session.
    pub session_id: SessionId,
    /// Mode to change to. It applies from the next turn.
    pub mode: Mode,
}

/// Result of [`SESSION_SET_MODE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSetModeResult {
    /// The session after the change.
    pub session: SessionInfo,
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

/// Parameters of [`DAEMON_BACKENDS`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DaemonBackendsParams {}

/// Result of [`DAEMON_BACKENDS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DaemonBackendsResult {
    /// The enabled backends, in the order the daemon registers them.
    pub backends: Vec<BackendKind>,
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
    /// Whether at least one backend is enabled. Absent from older daemons, which read as enabled.
    #[serde(default = "enabled_by_default")]
    pub any_provider_enabled: bool,
}

fn enabled_by_default() -> bool {
    true
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
    fn a_request_without_options_and_an_answer_without_a_choice_keep_their_old_wire_form() {
        let request = serde_json::json!({
            "session_id": "s", "request_id": "r", "tool": "bash",
            "repo_path": "/r", "command": "ls",
        });
        let answer = serde_json::json!({
            "session_id": "s", "request_id": "r", "decision": "deny",
        });

        let asked: PermissionRequestParams = serde_json::from_value(request.clone()).unwrap();
        let answered: PermissionRespondParams = serde_json::from_value(answer.clone()).unwrap();

        assert_eq!(serde_json::to_value(&asked).unwrap(), request);
        assert_eq!(serde_json::to_value(&answered).unwrap(), answer);
        assert_eq!(answered.choice, None);
        assert!(!answered.no_terminal);
    }

    #[test]
    fn wire_constants_match_the_contract() {
        // Arrange
        let expected = [
            (INITIALIZE, "initialize"),
            (SESSION_CREATE, "session.create"),
            (SESSION_SEND, "session.send"),
            (AGENT_RUN, "agent.run"),
            (SESSION_CANCEL, "session.cancel"),
            (SESSION_SUBSCRIBE, "session.subscribe"),
            (SESSION_RESUME, "session.resume"),
            (SESSION_LIST, "session.list"),
            (SESSION_CLOSE, "session.close"),
            (SESSION_REWIND, "session.rewind"),
            (SESSION_DIFF, "session.diff"),
            (SESSION_WORKTREE, "session.worktree"),
            (SESSION_SHIP, "session.ship"),
            (SESSION_WATCH_PR, "session.watch_pr"),
            (SCHEDULE_ADD, "schedule.add"),
            (SCHEDULE_LIST, "schedule.list"),
            (SCHEDULE_REMOVE, "schedule.remove"),
            (WORKTREE_CLEAN, "worktree.clean"),
            (CHECKPOINTS_PRUNE, "checkpoints.prune"),
            (PERMISSION_RESPOND, "permission.respond"),
            (GRANT_ADD, "grant.add"),
            (USAGE_SUMMARY, "usage.summary"),
            (SESSION_SET_MODE, "session.set_mode"),
            (SESSION_SET_MODEL, "session.set_model"),
            (DAEMON_BACKENDS, "daemon.backends"),
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
