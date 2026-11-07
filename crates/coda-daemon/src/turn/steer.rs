// SPDX-License-Identifier: Apache-2.0

//! `session.send` with `mode: steer`: adds a message to the turn that is running.
//!
//! Claude Code reads a user message written while it works and answers it in the
//! same turn. The Codex app server takes it as `turn/steer`. Coda's own loop adds
//! it to the conversation at its next step boundary. The Codex exec backend has
//! no way to take one, so a steer there is refused.

use std::collections::HashMap;
use std::sync::{Arc, MutexGuard, PoisonError};

use coda_protocol::methods::SessionSendParams;
use coda_protocol::types::{SessionId, TurnId};
use coda_providers::steer::SteerHandle;
use coda_store::EventRecord;
use serde_json::json;

use super::queue::Sent;
use super::{TurnError, TurnState, Turns, USER_MESSAGE_KIND, not_found_or};

/// Field of a stored `user_message` that marks a message steered into a turn.
pub(crate) const STEERED_FIELD: &str = "steered";

const NO_TURN: &str = "no turn is running to steer";
const CANNOT_STEER: &str = "this backend cannot take a message while a turn runs";

/// Where a message steered into one running agent turn goes.
#[derive(Debug, Clone)]
pub(super) struct AgentSteer {
    pub(super) turn: TurnId,
    /// `None` for a backend that cannot take a message mid-turn.
    pub(super) handle: Option<SteerHandle>,
}

impl Turns {
    pub(super) async fn send_steer(
        self: &Arc<Self>,
        params: SessionSendParams,
    ) -> Result<Sent, TurnError> {
        let SessionSendParams {
            session_id, text, ..
        } = params;
        let id = session_id.clone();
        self.hub
            .with_store(move |store| store.load_session(&id))
            .await
            .map_err(|e| not_found_or(e, &session_id))?;
        if self.turn_state(&session_id) != TurnState::Live {
            return Err(TurnError::Agent(NO_TURN.to_owned()));
        }
        let target = self.steers().get(&session_id).cloned();
        let turn = match target {
            Some(target) => self.steer_agent(&session_id, &text, target).await?,
            None => self
                .models
                .steer(&session_id, &text)
                .ok_or_else(|| TurnError::Agent(NO_TURN.to_owned()))?,
        };
        Ok(Sent {
            turn,
            queued: false,
            steered: true,
        })
    }

    async fn steer_agent(
        &self,
        session: &SessionId,
        text: &str,
        target: AgentSteer,
    ) -> Result<TurnId, TurnError> {
        let handle = target
            .handle
            .ok_or_else(|| TurnError::Agent(CANNOT_STEER.to_owned()))?;
        // Delivered first, so history never shows a message the agent did not get.
        if !handle.steer(text) {
            return Err(TurnError::Agent(NO_TURN.to_owned()));
        }
        let record = EventRecord {
            kind: USER_MESSAGE_KIND.to_owned(),
            body: json!({ "text": text, STEERED_FIELD: true }).to_string(),
        };
        self.hub.publish(session, &target.turn, record).await?;
        Ok(target.turn)
    }

    pub(super) fn steers(&self) -> MutexGuard<'_, HashMap<SessionId, AgentSteer>> {
        self.steers.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
