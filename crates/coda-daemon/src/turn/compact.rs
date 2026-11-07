// SPDX-License-Identifier: Apache-2.0

//! `session.compact`: compacts the conversation of an idle Claude Code session.
//!
//! The compaction is a call of its own (see `coda_providers::claude_compact`), so
//! the session must have no turn, and none starts while it runs.

use std::path::Path;
use std::sync::Arc;

use coda_protocol::methods::{SessionCompactParams, SessionCompactResult};
use coda_protocol::types::{BackendKind, SessionId};
use coda_providers::claude_compact::{CompactRequest, DEFAULT_TIMEOUT, compact};
use serde_json::Value;
use tracing::info;

use crate::backends::claude_env_passthrough;

use super::{
    HandlerResult, Reservation, TurnError, Turns, not_found_or, parse_params, to_value, unavailable,
};

const ONLY_CLAUDE_CODE: &str = "only a Claude Code session can be compacted";
const NO_CONVERSATION: &str = "the session has no conversation to compact yet";

impl Turns {
    /// Compacts the conversation of `session`, which keeps its id.
    ///
    /// # Errors
    /// Returns [`TurnError::SessionNotFound`], [`TurnError::Busy`] while a turn
    /// runs, and [`TurnError::Agent`] for a session that is not on Claude Code, has
    /// no conversation yet, or whose compaction fails.
    pub(crate) async fn compact(self: &Arc<Self>, session: &SessionId) -> Result<(), TurnError> {
        let id = session.clone();
        let record = self
            .hub
            .with_store(move |store| store.load_session(&id))
            .await
            .map_err(|e| not_found_or(e, session))?;
        let binary = match (&self.config.binary, record.backend) {
            (Some(binary), BackendKind::ClaudeCode) => binary.clone(),
            _ => return Err(TurnError::Agent(ONLY_CLAUDE_CODE.to_owned())),
        };
        self.await_wind_down(session).await?;
        let _hold = self.hold(&record)?;
        // There is nothing to cancel, so a stop must not claim to have taken.
        self.running()
            .insert(session.clone(), Reservation::uncancellable());
        let Some(backend_session) = self.resume_id(&record).await else {
            return Err(TurnError::Agent(NO_CONVERSATION.to_owned()));
        };
        self.drop_idle(session).await;
        info!(session = %session, "compacting the conversation");
        compact(&CompactRequest {
            binary: &binary,
            parent_env: &self.config.parent_env,
            env_passthrough: &claude_env_passthrough(&self.config.models),
            cwd: Path::new(&record.cwd),
            backend_session: &backend_session,
            timeout: DEFAULT_TIMEOUT,
        })
        .await
        .map_err(|e| TurnError::Agent(e.to_string()))
    }
}

/// Answers `session.compact`.
pub(crate) async fn session_compact(
    turns: Option<&Arc<Turns>>,
    params: Option<Value>,
) -> HandlerResult {
    let turns = turns.ok_or_else(unavailable)?;
    let SessionCompactParams { session_id } = parse_params(params)?;
    turns
        .compact(&session_id)
        .await
        .map_err(TurnError::into_rpc)?;
    to_value(&SessionCompactResult {})
}
