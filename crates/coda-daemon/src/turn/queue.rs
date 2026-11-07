// SPDX-License-Identifier: Apache-2.0

//! Follow-up messages: `session.send` with `mode: queue` and `session.queue`.
//!
//! A message sent while a turn runs is stored. When that turn ends cleanly the
//! oldest waiting message starts, and so on until none is left. A turn that was
//! stopped, failed or hit a limit starts nothing: the messages stay stored, and
//! stay after a restart, until the client resumes them.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use coda_protocol::methods::{
    QueueAction, QueuedMessage, SendMode, SessionQueueParams, SessionQueueResult, SessionSendParams,
};
use coda_protocol::types::{SessionId, TurnId};
use coda_store::{NewQueued, StoreError};
use serde_json::Value;
use tracing::{info, warn};

use crate::subscribe::HubError;

use super::{
    HandlerResult, TurnError, TurnState, Turns, not_found_or, parse_params, to_value, unavailable,
};

/// What a send did: started a turn, or queued behind one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    /// The turn started, or for a queued message the latest turn of the session.
    pub turn: TurnId,
    pub queued: bool,
    /// The message was added to the running turn.
    pub steered: bool,
}

impl Sent {
    pub(super) fn started(turn: TurnId) -> Self {
        Self {
            turn,
            queued: false,
            steered: false,
        }
    }
}

impl Turns {
    /// Starts the message now when the session is free and nothing waits,
    /// otherwise stores it behind the ones waiting.
    pub(super) async fn send_queued(
        self: &Arc<Self>,
        params: SessionSendParams,
    ) -> Result<Sent, TurnError> {
        let session = params.session_id.clone();
        let waiting = self.stored_waiting(&session).await?;
        let was_busy = self.turn_state(&session) != TurnState::Free;
        if !was_busy && waiting.is_empty() {
            return self.send_now(params).await.map(Sent::started);
        }
        let SessionSendParams {
            session_id: id,
            text,
            max_turns,
            token_cap,
            ..
        } = params;
        self.hub
            .with_store_mut(move |store| {
                store.enqueue_message(
                    &id,
                    NewQueued {
                        text: &text,
                        max_turns,
                        token_cap,
                    },
                )
            })
            .await
            .map_err(|e| match e {
                HubError::Store(StoreError::QueueFull { .. }) => {
                    TurnError::QueueFull(session.clone())
                }
                HubError::Store(e @ StoreError::QueuedTextTooLarge { .. }) => {
                    TurnError::Agent(e.to_string())
                }
                other => not_found_or(other, &session),
            })?;
        // The turn may have ended between the check and the write, after the
        // end already looked for a message to start.
        if was_busy && self.turn_state(&session) == TurnState::Free {
            Arc::clone(self).start_next_queued(session.clone());
        }
        let id = session.clone();
        let latest = self
            .hub
            .with_store(move |store| store.latest_turn(&id))
            .await?;
        let turn = latest.ok_or_else(|| TurnError::Busy(session))?;
        Ok(Sent {
            turn,
            queued: true,
            steered: false,
        })
    }

    /// Starts the next waiting message of `session` on its own task, once the
    /// caller is done with the session. A busy session keeps its messages: the
    /// turn that runs will start them when it ends.
    pub(crate) fn start_next_queued(self: Arc<Self>, session: SessionId) {
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        tokio::spawn(async move {
            match self.start_queued(&session).await {
                Ok(Some(turn)) => {
                    info!(session = %session, turn = %turn, "started a queued message");
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(session = %session, %error, "could not start a queued message");
                }
            }
        });
    }

    /// Starts the oldest waiting message. `None` when there is none or a turn runs.
    async fn start_queued(
        self: &Arc<Self>,
        session: &SessionId,
    ) -> Result<Option<TurnId>, TurnError> {
        // Two starts must not both read a message that the first one is still
        // starting, so the read, the start and the removal go one at a time.
        let _one_at_a_time = self.queue_gate.lock().await;
        let Some(next) = self.stored_waiting(session).await?.into_iter().next() else {
            return Ok(None);
        };
        let params = SessionSendParams {
            session_id: session.clone(),
            text: next.text,
            max_turns: next.max_turns,
            token_cap: next.token_cap,
            mode: SendMode::Start,
            client_request_id: None,
        };
        match self.send_now(params).await {
            Ok(turn) => {
                // A crash between the start and this removal sends the message again.
                self.remove_waiting(session, next.id).await?;
                Ok(Some(turn))
            }
            Err(TurnError::Busy(_) | TurnError::Ending(_)) => Ok(None),
            Err(other) => Err(other),
        }
    }

    async fn stored_waiting(
        &self,
        session: &SessionId,
    ) -> Result<Vec<coda_store::QueuedMessage>, TurnError> {
        let id = session.clone();
        self.hub
            .with_store(move |store| {
                store.load_session(&id)?;
                store.queued_messages(&id)
            })
            .await
            .map_err(|e| not_found_or(e, session))
    }

    async fn waiting(&self, session: &SessionId) -> Result<Vec<QueuedMessage>, TurnError> {
        Ok(self
            .stored_waiting(session)
            .await?
            .into_iter()
            .map(|m| QueuedMessage {
                id: m.id,
                text: m.text,
                at: m.at,
            })
            .collect())
    }

    async fn remove_waiting(&self, session: &SessionId, id: i64) -> Result<bool, TurnError> {
        let session = session.clone();
        Ok(self
            .hub
            .with_store_mut(move |store| store.remove_queued_message(&session, id))
            .await?)
    }

    /// Answers `session.queue`.
    ///
    /// # Errors
    /// Returns [`TurnError::SessionNotFound`], and the errors of starting a
    /// turn for a resume.
    pub(crate) async fn queue(
        self: &Arc<Self>,
        params: SessionQueueParams,
    ) -> Result<SessionQueueResult, TurnError> {
        let SessionQueueParams { session_id, action } = params;
        let started = match action {
            QueueAction::List => None,
            QueueAction::Resume => self.start_queued(&session_id).await?,
            QueueAction::Discard { queue_id } => {
                self.remove_waiting(&session_id, queue_id).await?;
                None
            }
        };
        let messages = self.waiting(&session_id).await?;
        Ok(SessionQueueResult { messages, started })
    }
}

/// Answers `session.queue`.
pub(crate) async fn session_queue(
    turns: Option<&Arc<Turns>>,
    params: Option<Value>,
) -> HandlerResult {
    let turns = turns.ok_or_else(unavailable)?;
    let result = turns
        .queue(parse_params(params)?)
        .await
        .map_err(TurnError::into_rpc)?;
    to_value(&result)
}
