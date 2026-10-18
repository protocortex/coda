// SPDX-License-Identifier: AGPL-3.0-only

//! The daemon link: what the UI asks of it and how it relays sessions.

use std::cmp::Reverse;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use coda_client::{Client, ClientError, Subscription};
use coda_config::timing::Timing;
use coda_protocol::events::{EVENT_NOTIFICATION, Event};
use coda_protocol::jsonrpc::Notification;
use coda_protocol::methods::{
    DAEMON_BACKENDS, DAEMON_PING, DaemonBackendsParams, DaemonBackendsResult, PERMISSION_RESPOND,
    PermissionRespondParams, PermissionRespondResult, PingParams, PingResult, SESSION_CANCEL,
    SESSION_CREATE, SESSION_LIST, SESSION_RESUME, SESSION_SEND, SESSION_SET_MODEL,
    SessionCancelParams, SessionCancelResult, SessionCreateParams, SessionCreateResult,
    SessionListParams, SessionListResult, SessionResumeParams, SessionResumeResult,
    SessionSendParams, SessionSendResult, SessionSetModelParams, SessionSetModelResult,
    SessionSummary,
};
use coda_protocol::types::{BackendKind, Mode, SessionId, SessionInfo};
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::warn;

use crate::app::LinkEvent;
use crate::permission::{PermissionAnswer, PermissionPrompt};
use crate::status::backend_name;

/// Notification that announces a request waiting for an answer.
const PERMISSION_REQUEST_NOTIFICATION: &str = "permission.request";
/// Stored kind of the user's own message, which the event stream carries as well.
const USER_MESSAGE_KIND: &str = "user_message";
/// How often a live link is probed, since the client reports a lost link only on use.
const LINK_PROBE_INTERVAL: Duration = Duration::from_millis(250);
const FALLBACK_CWD: &str = "/";
/// Backend of sessions the UI starts itself.
const DEFAULT_BACKEND: BackendKind = BackendKind::ClaudeCode;
/// A session idle for longer than this is not reopened on start.
const MAX_RESUME_AGE: Duration = Duration::from_hours(24);
/// Newest sessions of the working directory looked at when choosing one to reopen.
const RESUME_LOOKUP_LIMIT: u32 = 10;
/// Replaying from the first event rebuilds the transcript of a reopened session.
const REPLAY_FROM_START: u64 = 0;

/// Said after a backend switch: the new backend starts with an empty history.
pub const TRANSCRIPT_NOT_CARRIED: &str = "the transcript is not carried over to the new session";

/// A request from the UI to the task that holds the daemon connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkCommand {
    /// Send a user message to the current session.
    Send(String),
    /// Cancel the running turn of the current session.
    Cancel,
    /// Answer a permission request.
    Respond(PermissionAnswer),
    /// Start a fresh session and follow it.
    New,
    /// Reopen a stored session and follow it.
    Resume(SessionId),
    /// Ask the daemon for its sessions.
    ListSessions,
    /// Switch the current session to a model alias.
    SetModel(String),
    /// Show the backends the daemon offers.
    ListBackends,
    /// Start a new session on the named backend.
    SwitchBackend(String),
}

/// Reads an event notification's params; kinds the UI has no use for give `None`.
#[must_use]
pub fn event_from_params(params: &Value) -> Option<Event> {
    let body = params.get("body")?.as_str()?;
    serde_json::from_str(body).ok()
}

/// Reads the text of a stored user message notification; other kinds give `None`.
#[must_use]
pub fn user_message_from_params(params: &Value) -> Option<String> {
    if params.get("kind")?.as_str()? != USER_MESSAGE_KIND {
        return None;
    }
    let body: Value = serde_json::from_str(params.get("body")?.as_str()?).ok()?;
    body.get("text")?.as_str().map(str::to_owned)
}

fn backend_names(backends: &[BackendKind]) -> String {
    backends
        .iter()
        .map(|backend| backend_name(*backend))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The newest session of `sessions` that a start with `backend` may reopen:
/// the same backend and active within [`MAX_RESUME_AGE`] of `now_ms`.
/// `sessions` come newest first, which settles ties on the activity time.
fn resumable(
    sessions: Vec<SessionSummary>,
    backend: BackendKind,
    now_ms: i64,
) -> Option<SessionSummary> {
    let max_age_ms = i64::try_from(MAX_RESUME_AGE.as_millis()).unwrap_or(i64::MAX);
    sessions
        .into_iter()
        .min_by_key(|session| Reverse(session.updated_at))
        .filter(|session| {
            session.backend == backend && now_ms.saturating_sub(session.updated_at) <= max_age_ms
        })
}

fn now_ms() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// The two ways to begin following a session.
trait Opener {
    async fn resume(&mut self, id: &SessionId) -> Result<(), ClientError>;
    async fn create(&mut self) -> Result<(), ClientError>;
}

/// Reopens `candidate` when there is one and it opens, and otherwise starts a
/// new session. Returns the error when even that fails.
async fn open_session(
    opener: &mut impl Opener,
    candidate: Option<SessionId>,
) -> Result<Option<SessionId>, ClientError> {
    if let Some(id) = candidate {
        match opener.resume(&id).await {
            Ok(()) => return Ok(Some(id)),
            Err(error) => warn!(%error, session = %id, "could not reopen the latest session"),
        }
    }
    opener.create().await.map(|()| None)
}

/// What sessions the link starts.
#[derive(Debug, Clone)]
pub struct LinkConfig {
    pub backend: BackendKind,
    pub cwd: String,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            backend: DEFAULT_BACKEND,
            cwd: std::env::current_dir()
                .ok()
                .and_then(|dir| dir.to_str().map(str::to_owned))
                .unwrap_or_else(|| FALLBACK_CWD.to_owned()),
        }
    }
}

/// Connects, follows a session, and then relays between the UI and the daemon
/// until the UI hangs up or the link is lost.
pub async fn run_link(
    socket_path: PathBuf,
    timing: Timing,
    config: LinkConfig,
    mut commands: UnboundedReceiver<LinkCommand>,
    events: UnboundedSender<LinkEvent>,
) {
    let client = match Client::connect(&socket_path, timing).await {
        Ok(client) => client,
        Err(error) => {
            warn!(%error, "could not connect to the daemon");
            let _ = events.send(LinkEvent::Failed(error.to_string()));
            return;
        }
    };
    let _ = events.send(LinkEvent::Connected);
    let mut link = Link::new(client, config, events);
    link.start().await;
    let mut probe = tokio::time::interval(LINK_PROBE_INTERVAL);
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(command) => link.handle(command).await,
                None => return,
            },
            notification = next_notification(&mut link.subscription) => {
                if let Some(notification) = notification {
                    link.forward(&notification);
                } else {
                    link.subscription = None;
                    link.tell(LinkEvent::Disconnected("the session stream ended".to_owned()));
                }
            }
            _ = probe.tick() => {
                if let Err(error) = link.client
                    .request::<_, PingResult>(DAEMON_PING, &PingParams {})
                    .await
                {
                    warn!(%error, "lost the link to the daemon");
                    let _ = link.events.send(LinkEvent::Disconnected(error.to_string()));
                    return;
                }
            }
        }
    }
}

async fn next_notification(subscription: &mut Option<Subscription>) -> Option<Notification> {
    match subscription {
        Some(subscription) => subscription.next().await,
        None => std::future::pending().await,
    }
}

/// Opens streams of session events.
trait Subscribe {
    async fn subscribe(
        &self,
        session: &SessionId,
        since_seq: Option<u64>,
    ) -> Result<Subscription, ClientError>;
}

impl Subscribe for Client {
    async fn subscribe(
        &self,
        session: &SessionId,
        since_seq: Option<u64>,
    ) -> Result<Subscription, ClientError> {
        Client::subscribe(self, session, since_seq).await
    }
}

/// Moves `current` to the stream of `info`, then tells the UI so it clears the
/// screen before the replay. A failed subscribe leaves the UI and the stream it
/// follows untouched, except when the session is the one already followed: the
/// daemon allows one stream per session, so that one is closed first.
async fn follow_session(
    client: &impl Subscribe,
    current: &mut Option<Subscription>,
    events: &UnboundedSender<LinkEvent>,
    info: SessionInfo,
    since_seq: Option<u64>,
) -> Result<(), ClientError> {
    if current
        .as_ref()
        .is_some_and(|stream| stream.session() == &info.id)
    {
        *current = None;
    }
    let subscription = client.subscribe(&info.id, since_seq).await?;
    *current = Some(subscription);
    // The receiver is gone only while the UI shuts down.
    let _ = events.send(LinkEvent::SessionStarted(info));
    Ok(())
}

impl Opener for Link {
    async fn resume(&mut self, id: &SessionId) -> Result<(), ClientError> {
        self.reopen(id).await
    }

    async fn create(&mut self) -> Result<(), ClientError> {
        self.start_new().await
    }
}

struct Link {
    client: Client,
    config: LinkConfig,
    subscription: Option<Subscription>,
    events: UnboundedSender<LinkEvent>,
}

impl Link {
    fn new(client: Client, config: LinkConfig, events: UnboundedSender<LinkEvent>) -> Self {
        Self {
            client,
            config,
            subscription: None,
            events,
        }
    }

    fn tell(&self, event: LinkEvent) {
        // The receiver is gone only while the UI shuts down.
        let _ = self.events.send(event);
    }

    fn notice(&self, text: String) {
        self.tell(LinkEvent::Notice(text));
    }

    /// Reopens the recent session of the working directory that used the same
    /// backend, so a restarted UI continues where it stopped, or starts a new one.
    async fn start(&mut self) {
        let candidate = self.resume_candidate().await;
        match open_session(self, candidate).await {
            Ok(Some(id)) => self.notice(format!("resumed session {id}, use /new for a fresh one")),
            Ok(None) => {}
            Err(error) => self.notice(format!("could not start a session: {error}")),
        }
    }

    async fn list(
        &self,
        cwd: Option<String>,
        limit: Option<u32>,
    ) -> Result<Vec<SessionSummary>, ClientError> {
        let listed: SessionListResult = self
            .client
            .request(SESSION_LIST, &SessionListParams { cwd, limit })
            .await?;
        Ok(listed.sessions)
    }

    async fn resume_candidate(&self) -> Option<SessionId> {
        let listed = self
            .list(Some(self.config.cwd.clone()), Some(RESUME_LOOKUP_LIMIT))
            .await;
        match listed {
            Ok(sessions) => {
                resumable(sessions, self.config.backend, now_ms()).map(|session| session.id)
            }
            Err(error) => {
                warn!(%error, "could not list the stored sessions");
                None
            }
        }
    }

    async fn start_new(&mut self) -> Result<(), ClientError> {
        let params = SessionCreateParams {
            backend: self.config.backend,
            cwd: self.config.cwd.clone(),
            model: None,
            mode: Mode::default(),
        };
        let created: SessionCreateResult = self.client.request(SESSION_CREATE, &params).await?;
        self.follow(created.session, None).await
    }

    async fn reopen(&mut self, id: &SessionId) -> Result<(), ClientError> {
        let params = SessionResumeParams {
            session_id: id.clone(),
        };
        let resumed: SessionResumeResult = self.client.request(SESSION_RESUME, &params).await?;
        self.follow(resumed.session, Some(REPLAY_FROM_START)).await
    }

    async fn follow(
        &mut self,
        info: SessionInfo,
        since_seq: Option<u64>,
    ) -> Result<(), ClientError> {
        follow_session(
            &self.client,
            &mut self.subscription,
            &self.events,
            info,
            since_seq,
        )
        .await
    }

    async fn handle(&mut self, command: LinkCommand) {
        let outcome = match command {
            LinkCommand::Send(text) => self.send(text).await,
            LinkCommand::Cancel => self.cancel().await,
            LinkCommand::Respond(answer) => self.respond(answer).await,
            LinkCommand::ListSessions => self
                .list(None, None)
                .await
                .map(|sessions| self.tell(LinkEvent::Sessions(sessions)))
                .map_err(|e| ("list the sessions", e)),
            LinkCommand::SetModel(alias) => self.set_model(alias).await,
            LinkCommand::ListBackends => self.list_backends().await,
            LinkCommand::SwitchBackend(name) => self.switch_backend(&name).await,
            LinkCommand::New => self.start_new().await.map_err(|e| ("start a session", e)),
            LinkCommand::Resume(id) => self
                .reopen(&id)
                .await
                .map_err(|e| ("reopen the session", e)),
        };
        if let Err((action, error)) = outcome {
            self.notice(format!("could not {action}: {error}"));
        }
    }

    async fn backends(&self) -> Result<Vec<BackendKind>, ClientError> {
        let listed: DaemonBackendsResult = self
            .client
            .request(DAEMON_BACKENDS, &DaemonBackendsParams {})
            .await?;
        Ok(listed.backends)
    }

    async fn list_backends(&self) -> Result<(), (&'static str, ClientError)> {
        let backends = self
            .backends()
            .await
            .map_err(|e| ("list the backends", e))?;
        self.notice(format!("backends: {}", backend_names(&backends)));
        Ok(())
    }

    /// Starts a new session on `name` when the daemon offers that backend. The
    /// old session keeps its history; the new one starts empty.
    async fn switch_backend(&mut self, name: &str) -> Result<(), (&'static str, ClientError)> {
        let backends = self
            .backends()
            .await
            .map_err(|e| ("list the backends", e))?;
        let Some(chosen) = backends.iter().copied().find(|b| backend_name(*b) == name) else {
            self.notice(format!(
                "backend `{name}` is not available; choose one of: {}",
                backend_names(&backends)
            ));
            return Ok(());
        };
        let previous = std::mem::replace(&mut self.config.backend, chosen);
        if let Err(error) = self.start_new().await {
            self.config.backend = previous;
            return Err(("start a session on that backend", error));
        }
        self.notice(TRANSCRIPT_NOT_CARRIED.to_owned());
        Ok(())
    }

    async fn set_model(&self, alias: String) -> Result<(), (&'static str, ClientError)> {
        let Some(session_id) = self.current() else {
            self.notice("there is no session to switch".to_owned());
            return Ok(());
        };
        let params = SessionSetModelParams {
            session_id,
            model: alias,
        };
        let switched: SessionSetModelResult = self
            .client
            .request(SESSION_SET_MODEL, &params)
            .await
            .map_err(|e| ("switch the model", e))?;
        self.tell(LinkEvent::ModelSwitched(switched.session));
        Ok(())
    }

    fn current(&self) -> Option<SessionId> {
        self.subscription.as_ref().map(|s| s.session().clone())
    }

    async fn send(&self, text: String) -> Result<(), (&'static str, ClientError)> {
        let Some(session_id) = self.current() else {
            self.notice("there is no session to send to".to_owned());
            return Ok(());
        };
        let params = SessionSendParams {
            session_id,
            text,
            max_turns: None,
            token_cap: None,
        };
        self.client
            .request::<_, SessionSendResult>(SESSION_SEND, &params)
            .await
            .map(|_| ())
            .map_err(|e| ("send the message", e))
    }

    async fn cancel(&self) -> Result<(), (&'static str, ClientError)> {
        let Some(session_id) = self.current() else {
            return Ok(());
        };
        self.client
            .request::<_, SessionCancelResult>(SESSION_CANCEL, &SessionCancelParams { session_id })
            .await
            .map(|_| ())
            .map_err(|e| ("cancel the turn", e))
    }

    async fn respond(&self, answer: PermissionAnswer) -> Result<(), (&'static str, ClientError)> {
        let params = PermissionRespondParams {
            session_id: answer.session_id,
            request_id: answer.request_id,
            decision: answer.decision,
        };
        let result: PermissionRespondResult = self
            .client
            .request(PERMISSION_RESPOND, &params)
            .await
            .map_err(|e| ("answer the permission request", e))?;
        if !result.accepted {
            self.notice("the permission request was no longer waiting".to_owned());
        }
        Ok(())
    }

    fn forward(&self, notification: &Notification) {
        let Some(params) = notification.params.as_ref() else {
            return;
        };
        match notification.method.as_str() {
            EVENT_NOTIFICATION => {
                if let Some(text) = user_message_from_params(params) {
                    self.tell(LinkEvent::UserMessage(text));
                } else if let Some(event) = event_from_params(params) {
                    self.tell(LinkEvent::Session(event));
                }
            }
            PERMISSION_REQUEST_NOTIFICATION => {
                if let Some(prompt) = PermissionPrompt::from_params(params) {
                    self.tell(LinkEvent::Permission(prompt));
                } else {
                    warn!("dropping a malformed permission request");
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use coda_protocol::methods::SessionStatus;
    use serde_json::json;

    use super::*;

    const NOW: i64 = 10 * 24 * 60 * 60 * 1000;
    const MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000;

    fn summary(id: &str, backend: BackendKind, updated_at: i64) -> SessionSummary {
        SessionSummary {
            id: SessionId::new(id),
            backend,
            cwd: "/work".to_owned(),
            created_at: updated_at,
            updated_at,
            status: SessionStatus::Idle,
        }
    }

    #[test]
    fn the_newest_recent_session_of_the_same_backend_is_resumable() {
        // Arrange
        let sessions = vec![
            summary("1", BackendKind::Faux, NOW - 5_000),
            summary("2", BackendKind::Faux, NOW - 1_000),
        ];

        // Act
        let chosen = resumable(sessions, BackendKind::Faux, NOW);

        // Assert
        assert_eq!(chosen.map(|s| s.id), Some(SessionId::new("2")));
    }

    #[test]
    fn the_listed_first_session_wins_a_tie_on_activity_time() {
        // Arrange
        let sessions = vec![
            summary("2", BackendKind::Faux, NOW - 1_000),
            summary("1", BackendKind::Faux, NOW - 1_000),
        ];

        // Act
        let chosen = resumable(sessions, BackendKind::Faux, NOW);

        // Assert
        assert_eq!(chosen.map(|s| s.id), Some(SessionId::new("2")));
    }

    #[test]
    fn a_session_of_another_backend_is_not_resumable() {
        // Arrange
        let sessions = vec![summary("1", BackendKind::Faux, NOW - 1_000)];

        // Act
        let chosen = resumable(sessions, BackendKind::ClaudeCode, NOW);

        // Assert
        assert_eq!(chosen, None);
    }

    #[test]
    fn a_session_idle_past_the_limit_is_not_resumable_but_the_limit_itself_is() {
        // Arrange
        let at_limit = vec![summary("1", BackendKind::Faux, NOW - MAX_AGE_MS)];
        let past_limit = vec![summary("1", BackendKind::Faux, NOW - MAX_AGE_MS - 1)];

        // Act
        let kept = resumable(at_limit, BackendKind::Faux, NOW);
        let dropped = resumable(past_limit, BackendKind::Faux, NOW);

        // Assert
        assert!(kept.is_some());
        assert_eq!(dropped, None);
    }

    #[derive(Default)]
    struct FakeOpener {
        resume_fails: bool,
        calls: Vec<&'static str>,
    }

    #[allow(clippy::unused_async_trait_impl)]
    impl Opener for FakeOpener {
        async fn resume(&mut self, _id: &SessionId) -> Result<(), ClientError> {
            self.calls.push("resume");
            if self.resume_fails {
                Err(ClientError::Disconnected)
            } else {
                Ok(())
            }
        }

        async fn create(&mut self) -> Result<(), ClientError> {
            self.calls.push("create");
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_candidate_that_opens_is_resumed_without_creating() {
        // Arrange
        let mut opener = FakeOpener::default();

        // Act
        let outcome = open_session(&mut opener, Some(SessionId::new("4"))).await;

        // Assert
        assert_eq!(outcome.unwrap(), Some(SessionId::new("4")));
        assert_eq!(opener.calls, ["resume"]);
    }

    #[tokio::test]
    async fn a_candidate_that_fails_to_open_falls_back_to_a_new_session() {
        // Arrange
        let mut opener = FakeOpener {
            resume_fails: true,
            ..FakeOpener::default()
        };

        // Act
        let outcome = open_session(&mut opener, Some(SessionId::new("4"))).await;

        // Assert
        assert_eq!(outcome.unwrap(), None);
        assert_eq!(opener.calls, ["resume", "create"]);
    }

    struct FailingSubscribe;

    #[allow(clippy::unused_async_trait_impl)]
    impl Subscribe for FailingSubscribe {
        async fn subscribe(
            &self,
            _session: &SessionId,
            _since_seq: Option<u64>,
        ) -> Result<Subscription, ClientError> {
            Err(ClientError::Disconnected)
        }
    }

    #[tokio::test]
    async fn a_failed_subscribe_does_not_tell_the_ui_the_session_changed() {
        // Arrange
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let mut current = None;
        let info = SessionInfo {
            id: SessionId::new("s-new"),
            backend: BackendKind::Faux,
            model: None,
            cwd: "/work".to_owned(),
            mode: Mode::default(),
        };

        // Act
        let outcome = follow_session(&FailingSubscribe, &mut current, &events, info, None).await;

        // Assert
        assert!(outcome.is_err());
        assert!(current.is_none());
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn a_stored_event_notification_becomes_an_event() {
        // Arrange
        let body = json!({"type":"stream_delta","session_id":"s-1","turn_id":"t-1","text":"hi"});
        let params =
            json!({"session_id":"s-1","seq":3,"kind":"stream_delta","body":body.to_string()});

        // Act
        let event = event_from_params(&params);

        // Assert
        assert_eq!(
            event,
            Some(Event::StreamDelta {
                session_id: SessionId::new("s-1"),
                turn_id: coda_protocol::types::TurnId::new("t-1"),
                text: "hi".to_owned(),
            })
        );
    }
}
