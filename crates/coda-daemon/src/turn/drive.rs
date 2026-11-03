// SPDX-License-Identifier: Apache-2.0

//! One turn's runs of the backend, restarted by the supervisor when it crashes.

use std::sync::atomic::Ordering;

use coda_protocol::events::{Event, InterruptCause};
use coda_protocol::types::TurnId;
use coda_providers::claude_session::TurnOutcome;
use coda_providers::codex::{Carried, seeded_prompt};
use coda_store::SessionRecord;
use tokio::sync::watch;
use tracing::{info, warn};

use super::{CancelHandle, HISTORY_UNREAD_REASON, Turns, WatchedGate, remove_pidfile, stop};
use crate::backends::{AgentError, AgentSession};
use crate::supervisor::{Attempt, Step};
use crate::watchdog::Watchdog;

/// Waits for the backend to report a new conversation id. Never ends when it cannot.
async fn reported(updates: &mut Option<watch::Receiver<Option<String>>>) -> String {
    if let Some(receiver) = updates
        && receiver.changed().await.is_ok()
        && let Some(id) = receiver.borrow_and_update().clone()
    {
        return id;
    }
    std::future::pending().await
}

/// How the last run of the backend ended. `None` means it crashed and could not be started again.
pub(super) type Outcome = Option<Result<TurnOutcome, AgentError>>;

pub(super) struct Drive<'a, E> {
    pub turns: &'a Turns,
    pub record: &'a SessionRecord,
    pub turn: &'a TurnId,
    pub text: &'a str,
    pub watched: &'a WatchedGate,
    pub watchdog: &'a Watchdog,
    pub emit: E,
    pub child: AgentSession,
    pub outcome: Outcome,
    pub timed_out: bool,
    /// The earlier conversation, sent before the prompt once a resume was refused.
    seed: Option<String>,
    /// The cancel handle of the run that crashed, so a cancel that raced the crash is not lost.
    crashed_cancel: Option<CancelHandle>,
}

impl<'a, E: FnMut(Event) + Send> Drive<'a, E> {
    pub fn new(
        turns: &'a Turns,
        record: &'a SessionRecord,
        turn: &'a TurnId,
        text: &'a str,
        (watched, watchdog): (&'a WatchedGate, &'a Watchdog),
        (child, emit): (AgentSession, E),
    ) -> Self {
        Self {
            turns,
            record,
            turn,
            text,
            watched,
            watchdog,
            emit,
            child,
            outcome: None,
            timed_out: false,
            seed: None,
            crashed_cancel: None,
        }
    }

    /// Ends the turn as failed. The cause goes to the log: a result event carries none.
    fn fail_turn(&mut self) {
        (self.emit)(Event::Result {
            session_id: self.record.id.clone(),
            turn_id: self.turn.clone(),
            is_error: true,
        });
    }

    fn interrupted(&mut self, cause: InterruptCause) {
        (self.emit)(Event::TurnInterrupted {
            session_id: self.record.id.clone(),
            turn_id: self.turn.clone(),
            cause: Some(cause),
        });
    }

    /// Starts the crashed session's backend again, unless the user cancelled or
    /// the daemon is shutting down in the meantime.
    async fn restart(&mut self) -> bool {
        let crashed = self.crashed_cancel.take();
        let cancelled = || crashed.as_ref().is_some_and(CancelHandle::is_cancelled);
        let stopping = || self.turns.stopping.load(Ordering::SeqCst);
        let stopped = || cancelled() || stopping();
        if stopped() {
            // A user's cancel outranks the shutdown that raced it.
            let cause = if cancelled() {
                InterruptCause::Cancelled
            } else {
                InterruptCause::DaemonStopping
            };
            self.interrupted(cause);
            self.outcome = Some(Ok(TurnOutcome::Cancelled));
            return false;
        }
        match self.turns.start_child(self.record).await {
            Ok(fresh) => self.child = fresh,
            Err(e) => {
                warn!(session = %self.record.id, turn = %self.turn, error = %e, "could not restart the backend; failing the turn");
                self.outcome = None;
                self.fail_turn();
                return false;
            }
        }
        let cancel = self.child.cancel_handle();
        if let Some(reservation) = self.turns.running().get_mut(&self.record.id) {
            reservation.rearm(cancel.clone());
        }
        // A cancel that landed after the check above hit the old handle, so hand it on.
        if stopped() {
            cancel.cancel();
        }
        true
    }

    /// Reads the stored conversation, so the fresh start can be told it with the prompt.
    async fn seed_fresh_start(&mut self) {
        match self.turns.carried(&self.record.id, Some(self.turn)).await {
            Carried::Seed(history) => self.seed = Some(history),
            Carried::Lost => (self.emit)(Event::HistoryNotRestored {
                session_id: self.record.id.clone(),
                reason: HISTORY_UNREAD_REASON.to_owned(),
            }),
            Carried::Nothing => {}
        }
    }

    async fn run_once(&mut self, may_restart: bool) -> Step {
        let session = &self.record.id;
        let cancel = self.child.cancel_handle();
        // The crash event waits until it is known whether a restart follows.
        let mut crash = None;
        let mut forward = |event: Event| {
            if matches!(event, Event::BackendCrashed { .. }) {
                crash = Some(event);
            } else {
                (self.emit)(event);
            }
        };
        let prompt = match &self.seed {
            Some(history) => seeded_prompt(history, self.text),
            None => self.text.to_owned(),
        };
        let (turns, record, turn) = (self.turns, self.record, self.turn);
        let mut ids = self.child.backend_id_updates();
        let result = {
            let run = self
                .child
                .run_turn(turn, &prompt, self.watched, &mut forward);
            tokio::pin!(run);
            // The id is stored as soon as the backend names its conversation, so a
            // daemon killed mid-turn can still resume it.
            loop {
                tokio::select! {
                    result = &mut run => break result,
                    phase = self.watchdog.expired() => {
                        warn!(session = %session, turn = %turn, ?phase, "backend silent for too long; aborting");
                        self.timed_out = true;
                        break Ok(TurnOutcome::Cancelled);
                    }
                    id = reported(&mut ids) => turns.remember_backend_id(record, turn, &id).await,
                }
            }
        };
        if self.timed_out {
            self.interrupted(InterruptCause::IdleTimeout);
        }
        // A crash that raced the user's cancel is the user's choice, not a failure to retry.
        let crashed = matches!(result, Ok(TurnOutcome::Crashed { .. }))
            && !cancel.is_cancelled()
            && !self.turns.stopping.load(Ordering::SeqCst);
        // Starting again resends the prompt, so it is safe only before any tool was
        // asked for, and only for a backend that holds no state of its own.
        let replayable = !self.watched.tool_requested() && self.child.restartable();
        let will_restart = crashed && may_restart && replayable;
        if let Some(Event::BackendCrashed {
            session_id,
            exit_code,
            detail,
            ..
        }) = crash
        {
            (self.emit)(Event::BackendCrashed {
                session_id,
                exit_code,
                will_restart,
                detail,
            });
        }
        self.outcome = Some(result);
        if crashed && !replayable {
            warn!(session = %self.record.id, turn = %self.turn, "the backend crashed and the prompt cannot be replayed; failing the turn");
            self.fail_turn();
        }
        if !crashed || !replayable {
            return Step::Finished;
        }
        // The next run resumes the backend's conversation, and the old group must be gone first.
        let session = &self.record.id;
        if let Some(backend_id) = self.child.session_id_from_backend() {
            self.turns
                .remember_backend_id(self.record, self.turn, backend_id)
                .await;
        } else if self.turns.forget_backend_id(self.record).await {
            // A backend that dies before it names its conversation was refused its resume.
            warn!(session = %session, turn = %self.turn, "the backend did not accept the stored conversation id; starting it fresh");
            self.seed_fresh_start().await;
        }
        stop(&mut self.child, session).await;
        remove_pidfile(&self.turns.config.pidfile_dir, session);
        self.crashed_cancel = Some(cancel);
        Step::Crashed
    }
}

impl<E: FnMut(Event) + Send> Attempt for Drive<'_, E> {
    async fn attempt(&mut self, restarts: u32, may_restart: bool) -> Step {
        if restarts > 0 {
            info!(session = %self.record.id, turn = %self.turn, restarts, step = "restart", "restarting the crashed backend");
            if !self.restart().await {
                return Step::Finished;
            }
            self.watchdog.reset();
        }
        self.run_once(may_restart).await
    }
}
