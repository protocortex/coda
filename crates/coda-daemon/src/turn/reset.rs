// SPDX-License-Identifier: Apache-2.0

//! `session.resume_at_reset`: a session that opted in is continued once the plan
//! window that stopped it resets.
//!
//! A turn that ends at the plan limit leaves the session's queue held (see the
//! queue module). When the session opted in, the daemon waits for the reset time
//! the provider reported, then sends [`RESUME_PROMPT`]. The send carries a request
//! id made of the session, the turn and the reset time, so the same reset of the
//! same turn never sends it twice. A new message, a stop or an opt-out cancels the
//! wait. The opt-in and the wait live in memory: a daemon restart drops both.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use coda_protocol::methods::{
    SESSION_SEND, SendMode, SessionResumeAtResetParams, SessionResumeAtResetResult,
    SessionSendParams,
};
use coda_protocol::types::{SessionId, TurnId};
use serde_json::Value;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::receipts;

use super::{HandlerResult, TurnError, Turns, not_found_or, parse_params, to_value, unavailable};

/// What the agent is sent when its plan window resets.
pub const RESUME_PROMPT: &str = "Continue where you left off";

/// How long past the reset time the daemon waits, so the provider has let the
/// window go before the message is sent.
const RESET_MARGIN: Duration = Duration::from_secs(5);

/// Where the daemon reads the time, and how long it waits past a reset. The
/// default is the system clock; tests shorten both.
#[derive(Clone, Copy)]
pub(crate) struct ResetPolicy {
    /// Seconds since the Unix epoch.
    pub(crate) now: fn() -> u64,
    pub(crate) margin: Duration,
}

impl Default for ResetPolicy {
    fn default() -> Self {
        Self {
            now: epoch_seconds,
            margin: RESET_MARGIN,
        }
    }
}

fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The sessions that opted in, and the resumes that are waiting.
#[derive(Default)]
pub(crate) struct ResetState {
    opted_in: HashSet<SessionId>,
    /// Each wait with a number, so a task can tell that a newer wait replaced it.
    waiting: HashMap<SessionId, (u64, JoinHandle<()>)>,
    next_wait: u64,
    policy: ResetPolicy,
}

impl ResetState {
    fn cancel(&mut self, session: &SessionId) -> bool {
        match self.waiting.remove(session) {
            Some((_, handle)) => {
                handle.abort();
                true
            }
            None => false,
        }
    }

    fn is_current(&self, session: &SessionId, wait: u64) -> bool {
        self.opted_in.contains(session)
            && self
                .waiting
                .get(session)
                .is_some_and(|(number, _)| *number == wait)
    }
}

impl Turns {
    fn reset_state(&self) -> std::sync::MutexGuard<'_, ResetState> {
        self.reset
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn set_reset_policy(&self, policy: ResetPolicy) {
        self.reset_state().policy = policy;
    }

    #[cfg(test)]
    pub(crate) fn resume_scheduled(&self, session: &SessionId) -> bool {
        self.reset_state().waiting.contains_key(session)
    }

    #[cfg(test)]
    pub(crate) fn resume_opted_in(&self, session: &SessionId) -> bool {
        self.reset_state().opted_in.contains(session)
    }

    /// Forgets the opt-in and the wait of a closed session.
    pub(super) fn forget_resume(&self, session: &SessionId) {
        let mut state = self.reset_state();
        state.opted_in.remove(session);
        state.cancel(session);
    }

    /// Ends every wait, for a daemon that is going down.
    pub(super) fn cancel_all_resumes(&self) {
        let mut state = self.reset_state();
        state.opted_in.clear();
        for (_, (_, handle)) in state.waiting.drain() {
            handle.abort();
        }
    }

    /// Turns the opt-in on or off for `session`. Turning it off cancels a waiting
    /// resume.
    ///
    /// # Errors
    /// Returns [`TurnError::SessionNotFound`] for an unknown session.
    pub(crate) async fn set_resume_at_reset(
        &self,
        session: &SessionId,
        enabled: bool,
    ) -> Result<bool, TurnError> {
        let id = session.clone();
        self.hub
            .with_store(move |store| store.load_session(&id))
            .await
            .map_err(|e| not_found_or(e, session))?;
        let mut state = self.reset_state();
        if enabled {
            state.opted_in.insert(session.clone());
        } else {
            state.opted_in.remove(session);
            state.cancel(session);
        }
        Ok(enabled)
    }

    /// Cancels the resume `session` is waiting for, when it has one. A new message
    /// or a stop calls this.
    pub(super) fn cancel_resume(&self, session: &SessionId) {
        if self.reset_state().cancel(session) {
            info!(session = %session, "cancelled the resume at reset");
        }
    }

    /// Waits for the reset of the window a turn hit, then continues the session,
    /// when it opted in and the provider named a reset time.
    pub(super) fn note_usage_limit(
        self: &Arc<Self>,
        session: &SessionId,
        turn: &TurnId,
        reset_at: u64,
    ) {
        let mut state = self.reset_state();
        if !state.opted_in.contains(session) {
            return;
        }
        let policy = state.policy;
        let remaining = reset_at.saturating_sub((policy.now)());
        if remaining == 0 {
            warn!(session = %session, reset_at, "the reported reset time has passed; not waiting for it");
            return;
        }
        let delay = Duration::from_secs(remaining) + policy.margin;
        let wait = state.next_wait;
        state.next_wait += 1;
        let (turns, id, turn) = (Arc::clone(self), session.clone(), turn.clone());
        let handle = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            // Behind any send that holds the gate, so a stop or a message that
            // came in meanwhile has already cancelled this wait.
            let gate = turns.request_gate.lock().await;
            let current = {
                let mut state = turns.reset_state();
                let current = state.is_current(&id, wait);
                if current {
                    state.waiting.remove(&id);
                }
                current
            };
            if !current || turns.stopping.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            if let Err(error) = turns.resume_locked(&id, &turn, reset_at).await {
                warn!(session = %id, %error, "could not resume at the reset");
            }
            drop(gate);
        });
        if let Some((_, earlier)) = state.waiting.insert(session.clone(), (wait, handle)) {
            earlier.abort();
        }
        info!(session = %session, reset_at, "waiting for the plan window to reset");
    }

    /// Sends [`RESUME_PROMPT`] for this reset of this turn, unless that send was
    /// already made. Returns the turn it started, or `None` for a repeat.
    ///
    /// # Errors
    /// Returns the errors of starting a turn, and [`TurnError::Agent`] when the
    /// receipt of an earlier send cannot be read.
    #[cfg(test)]
    pub(crate) async fn resume_now(
        self: &Arc<Self>,
        session: &SessionId,
        turn: &TurnId,
        reset_at: u64,
    ) -> Result<Option<TurnId>, TurnError> {
        let _one_at_a_time = self.request_gate.lock().await;
        self.resume_locked(session, turn, reset_at).await
    }

    /// [`Self::resume_now`] for a caller that holds the request gate.
    async fn resume_locked(
        self: &Arc<Self>,
        session: &SessionId,
        turn: &TurnId,
        reset_at: u64,
    ) -> Result<Option<TurnId>, TurnError> {
        let request = format!("resume-{session}-{turn}-{reset_at}");
        receipts::check_id(&request).map_err(|e| TurnError::Agent(e.message))?;
        let seen = receipts::replay(&self.hub, &request, SESSION_SEND, Some(session))
            .await
            .map_err(|e| TurnError::Agent(e.message))?;
        if seen.is_some() {
            return Ok(None);
        }
        let params = SessionSendParams {
            session_id: session.clone(),
            text: RESUME_PROMPT.to_owned(),
            max_turns: None,
            token_cap: None,
            mode: SendMode::Start,
            client_request_id: Some(request),
        };
        self.send_now(params).await.map(Some)
    }
}

/// Answers `session.resume_at_reset`.
pub(crate) async fn resume_at_reset(
    turns: Option<&Arc<Turns>>,
    params: Option<Value>,
) -> HandlerResult {
    let turns = turns.ok_or_else(unavailable)?;
    let SessionResumeAtResetParams {
        session_id,
        enabled,
    } = parse_params(params)?;
    let enabled = turns
        .set_resume_at_reset(&session_id, enabled)
        .await
        .map_err(TurnError::into_rpc)?;
    to_value(&SessionResumeAtResetResult { enabled })
}
