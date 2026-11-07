// SPDX-License-Identifier: Apache-2.0

//! `session.fork`: starts a new session from a turn of an existing one.
//!
//! The fork is a session of its own with the backend, model, folder and mode of
//! its parent. Nothing is started here. The backend conversation begins on the
//! fork's first send, which tells the backend what the parent's turns up to the
//! fork point said, as text, the way a conversation that was rewound is told.
//! After that first turn the backend keeps the fork's conversation itself.

use std::sync::Arc;

use coda_protocol::methods::{SessionForkParams, SessionForkResult};
use coda_protocol::types::{BackendKind, SessionId, TurnId};
use coda_store::StoreError;

use super::{HubError, TurnError, Turns, not_found_or};
use crate::session_rpc::info;

const BACKEND_NOT_SUPPORTED: &str =
    "only a session on Claude Code or Codex can be forked; a model-API session cannot yet";
const NO_TURN: &str = "the session has no turn to fork";
const UNKNOWN_TURN: &str = "that turn is not part of the session";
const ROLLED_BACK_TURN: &str = "that turn is rolled back, so it cannot be a fork point";

impl Turns {
    /// Makes a new session from `params.turn_id` of `params.session_id`, or from
    /// the latest turn that is not rolled back.
    ///
    /// # Errors
    /// Returns [`TurnError::SessionNotFound`] and [`TurnError::Fork`] when the
    /// session cannot be forked as asked. Nothing is made then.
    pub async fn fork(
        self: &Arc<Self>,
        params: SessionForkParams,
    ) -> Result<SessionForkResult, TurnError> {
        let SessionForkParams {
            session_id,
            turn_id,
        } = params;
        let id = session_id.clone();
        let record = self
            .hub
            .with_store(move |store| store.load_session(&id))
            .await
            .map_err(|e| not_found_or(e, &session_id))?;
        if !matches!(
            record.backend,
            BackendKind::ClaudeCode | BackendKind::CodexAppServer
        ) {
            return Err(TurnError::Fork(BACKEND_NOT_SUPPORTED.to_owned()));
        }
        let source = self.fork_point(&session_id, turn_id).await?;
        let (parent, turn) = (session_id.clone(), source.clone());
        let made = self
            .hub
            .with_store_mut(move |store| {
                let fork = store.create_fork(&parent, &turn)?;
                store.load_session(&fork)
            })
            .await;
        let fork = made.map_err(|e| match e {
            HubError::Store(StoreError::TurnNotFound { .. }) => {
                TurnError::Fork(UNKNOWN_TURN.to_owned())
            }
            other => TurnError::Hub(other),
        })?;
        tracing::info!(session = %session_id, fork = %fork.id, turn = %source, "forked the session");
        Ok(SessionForkResult {
            session: info(fork),
            parent_session_id: session_id,
            source_turn: source,
        })
    }

    /// The turn the fork keeps up to: the one asked for, which must not be rolled
    /// back, else the latest turn that is not.
    async fn fork_point(
        &self,
        session: &SessionId,
        asked: Option<TurnId>,
    ) -> Result<TurnId, TurnError> {
        let id = session.clone();
        let (checkpoints, rolled_back) = self
            .hub
            .with_store(move |store| {
                Ok((store.turn_checkpoints(&id)?, store.rolled_back_turns(&id)?))
            })
            .await?;
        match asked {
            Some(turn) if rolled_back.contains(&turn) => {
                Err(TurnError::Fork(ROLLED_BACK_TURN.to_owned()))
            }
            Some(turn) => Ok(turn),
            None => checkpoints
                .into_iter()
                .map(|c| c.turn)
                .rfind(|turn| !rolled_back.contains(turn))
                .ok_or_else(|| TurnError::Fork(NO_TURN.to_owned())),
        }
    }
}
