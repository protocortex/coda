// SPDX-License-Identifier: AGPL-3.0-only

//! The daemon link: what the UI asks of it and how it relays sessions.

use std::path::PathBuf;
use std::time::Duration;

use coda_client::{Client, ClientError, Subscription};
use coda_config::timing::Timing;
use coda_protocol::events::{EVENT_NOTIFICATION, Event};
use coda_protocol::jsonrpc::Notification;
use coda_protocol::methods::{
    DAEMON_PING, PERMISSION_RESPOND, PermissionRespondParams, PermissionRespondResult, PingParams,
    PingResult, SESSION_CANCEL, SESSION_CREATE, SESSION_RESUME, SESSION_SEND, SessionCancelParams,
    SessionCancelResult, SessionCreateParams, SessionCreateResult, SessionResumeParams,
    SessionResumeResult, SessionSendParams, SessionSendResult,
};
use coda_protocol::types::{BackendKind, Mode, SessionId, SessionInfo};
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::warn;

use crate::app::LinkEvent;
use crate::permission::{PermissionAnswer, PermissionPrompt};

/// Notification that announces a request waiting for an answer.
const PERMISSION_REQUEST_NOTIFICATION: &str = "permission.request";
/// How often a live link is probed, since the client reports a lost link only on use.
const LINK_PROBE_INTERVAL: Duration = Duration::from_millis(250);
const FALLBACK_CWD: &str = "/";
/// Backend of sessions the UI starts itself.
const DEFAULT_BACKEND: BackendKind = BackendKind::ClaudeCode;
/// Replaying from the first event rebuilds the transcript of a reopened session.
const REPLAY_FROM_START: u64 = 0;

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
}

/// Reads an event notification's params; kinds the UI has no use for give `None`.
#[must_use]
pub fn event_from_params(params: &Value) -> Option<Event> {
    let body = params.get("body")?.as_str()?;
    serde_json::from_str(body).ok()
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

struct Link {
    client: Client,
    config: LinkConfig,
    known: Vec<SessionInfo>,
    subscription: Option<Subscription>,
    events: UnboundedSender<LinkEvent>,
}

impl Link {
    fn new(client: Client, config: LinkConfig, events: UnboundedSender<LinkEvent>) -> Self {
        Self {
            client,
            config,
            known: Vec::new(),
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

    async fn start(&mut self) {
        if let Err(error) = self.create().await {
            self.notice(format!("could not start a session: {error}"));
        }
    }

    async fn create(&mut self) -> Result<(), ClientError> {
        let params = SessionCreateParams {
            backend: self.config.backend,
            cwd: self.config.cwd.clone(),
            model: None,
            mode: Mode::default(),
        };
        let created: SessionCreateResult = self.client.request(SESSION_CREATE, &params).await?;
        self.follow(created.session, None).await
    }

    async fn resume(&mut self, id: &SessionId) -> Result<(), ClientError> {
        let params = SessionResumeParams {
            session_id: id.clone(),
        };
        let resumed: SessionResumeResult = self.client.request(SESSION_RESUME, &params).await?;
        self.follow(resumed.session, Some(REPLAY_FROM_START)).await
    }

    /// Switches to `info`. The UI is told first so it clears the screen before the replay.
    async fn follow(
        &mut self,
        info: SessionInfo,
        since_seq: Option<u64>,
    ) -> Result<(), ClientError> {
        self.subscription = None;
        self.tell(LinkEvent::SessionStarted(info.clone()));
        self.subscription = Some(self.client.subscribe(&info.id, since_seq).await?);
        if !self.known.iter().any(|known| known.id == info.id) {
            self.known.push(info);
        }
        self.tell(LinkEvent::Sessions(self.known.clone()));
        Ok(())
    }

    async fn handle(&mut self, command: LinkCommand) {
        let outcome = match command {
            LinkCommand::Send(text) => self.send(text).await,
            LinkCommand::Cancel => self.cancel().await,
            LinkCommand::Respond(answer) => self.respond(answer).await,
            LinkCommand::New => self.create().await.map_err(|e| ("start a session", e)),
            LinkCommand::Resume(id) => self
                .resume(&id)
                .await
                .map_err(|e| ("reopen the session", e)),
        };
        if let Err((action, error)) = outcome {
            self.notice(format!("could not {action}: {error}"));
        }
    }

    fn current(&self) -> Option<SessionId> {
        self.subscription.as_ref().map(|s| s.session().clone())
    }

    async fn send(&self, text: String) -> Result<(), (&'static str, ClientError)> {
        let Some(session_id) = self.current() else {
            self.notice("there is no session to send to".to_owned());
            return Ok(());
        };
        let params = SessionSendParams { session_id, text };
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
                // Kinds the UI does not show, such as the user's own message, are skipped.
                if let Some(event) = event_from_params(params) {
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
    use serde_json::json;

    use super::*;

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
