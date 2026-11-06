// SPDX-License-Identifier: Apache-2.0

//! One turn's runs of the backend, restarted by the supervisor when it crashes.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::time::Duration;

use coda_protocol::events::{Event, InterruptCause};
use coda_protocol::types::TurnId;
use coda_providers::claude_session::TurnOutcome;
use coda_providers::codex::{Carried, seeded_prompt};
use coda_providers::emit::Emit;
use coda_store::SessionRecord;
use tokio::sync::watch;
use tracing::{info, warn};

use super::{CancelHandle, HISTORY_UNREAD_REASON, Turns, WatchedGate, remove_pidfile, stop};
use crate::backends::{AgentError, AgentSession};
use crate::supervisor::{Attempt, Step};
use crate::watchdog::Watchdog;

/// Crashes before the backend names its conversation, with one stored id to resume,
/// after which the id is taken as refused.
const EARLY_CRASHES_BEFORE_FRESH: u32 = 2;

type Write<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

fn save_id<'a>(turns: &'a Turns, record: &'a SessionRecord, id: String) -> Write<'a> {
    Box::pin(async move { turns.remember_backend_id(record, &id).await })
}

/// Ends the id writes a run left open. After an abort a store that does not answer
/// must not hold the session, so the wait has a limit then.
async fn finish_writes(
    turns: &Turns,
    record: &SessionRecord,
    turn: &TurnId,
    (saving, waiting): (Option<Write<'_>>, Option<String>),
    limit: Option<Duration>,
) {
    let settle = async {
        if let Some(write) = saving {
            write.await;
        }
        if let Some(id) = waiting {
            turns.remember_backend_id(record, &id).await;
        }
    };
    match limit {
        Some(limit) => {
            if tokio::time::timeout(limit, settle).await.is_err() {
                warn!(session = %record.id, turn = %turn, "the store did not answer; the backend id was not saved");
            }
        }
        None => settle.await,
    }
}

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

/// Passes the adapter's events on, keeping the crash back until it is known whether a restart follows.
struct Forward<'a, E> {
    crash: &'a mut Option<Event>,
    emit: &'a mut E,
}

impl<E: Emit> Emit for Forward<'_, E> {
    async fn emit(&mut self, event: Event) {
        if matches!(event, Event::BackendCrashed { .. }) {
            *self.crash = Some(event);
        } else {
            self.emit.emit(event).await;
        }
    }
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
    /// Runs that crashed before the backend named its conversation while resuming one.
    early_crashes: u32,
    /// The next start does not resume: the stored conversation is taken as refused.
    fresh_start: bool,
    /// The cancel handle of the run that crashed, so a cancel that raced the crash is not lost.
    crashed_cancel: Option<CancelHandle>,
}

impl<'a, E: Emit> Drive<'a, E> {
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
            early_crashes: 0,
            fresh_start: false,
            crashed_cancel: None,
        }
    }

    /// Ends the turn as failed. The cause goes to the log: a result event carries none.
    async fn fail_turn(&mut self) {
        self.emit
            .emit(Event::Result {
                session_id: self.record.id.clone(),
                turn_id: self.turn.clone(),
                is_error: true,
            })
            .await;
    }

    async fn interrupted(&mut self, cause: InterruptCause) {
        self.emit
            .emit(Event::TurnInterrupted {
                session_id: self.record.id.clone(),
                turn_id: self.turn.clone(),
                cause: Some(cause),
            })
            .await;
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
            self.interrupted(cause).await;
            self.outcome = Some(Ok(TurnOutcome::Cancelled));
            return false;
        }
        match self.turns.start_child(self.record, self.fresh_start).await {
            Ok(fresh) => self.child = fresh,
            Err(e) => {
                warn!(session = %self.record.id, turn = %self.turn, error = %e, "could not restart the backend; failing the turn");
                self.outcome = None;
                self.fail_turn().await;
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
            Carried::Lost => {
                self.emit
                    .emit(Event::HistoryNotRestored {
                        session_id: self.record.id.clone(),
                        reason: HISTORY_UNREAD_REASON.to_owned(),
                    })
                    .await;
            }
            Carried::Nothing => {}
        }
    }

    /// Before a restart: keeps the id the backend named, or counts a crash that
    /// came before it named any while an id was being resumed.
    async fn note_conversation(&mut self) {
        let session = &self.record.id;
        if let Some(backend_id) = self.child.session_id_from_backend() {
            self.turns
                .remember_backend_id(self.record, backend_id)
                .await;
            // The conversation now holds the earlier turns, so a retry must not send them again.
            self.seed = None;
            self.fresh_start = false;
            self.early_crashes = 0;
        } else if !self.fresh_start && self.turns.resume_id(self.record).await.is_some() {
            self.early_crashes += 1;
            if self.early_crashes >= EARLY_CRASHES_BEFORE_FRESH {
                // The same id failed twice in a row, which a passing fault does not do.
                warn!(session = %session, turn = %self.turn, "the backend did not accept the stored conversation id; starting it fresh");
                self.turns.forget_resume_id(self.record);
                self.fresh_start = true;
                self.seed_fresh_start().await;
            }
        }
    }

    async fn run_once(&mut self, may_restart: bool) -> Step {
        let session = &self.record.id;
        let cancel = self.child.cancel_handle();
        // The crash event waits until it is known whether a restart follows.
        let mut crash = None;
        let mut forward = Forward {
            crash: &mut crash,
            emit: &mut self.emit,
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
            // daemon killed mid-turn can still resume it. The write runs beside the
            // backend instead of holding the loop, and the latest id waits its turn.
            let mut saving: Option<Write<'_>> = None;
            let mut waiting: Option<String> = None;
            let finished = loop {
                tokio::select! {
                    result = &mut run => break result,
                    phase = self.watchdog.expired() => {
                        warn!(session = %session, turn = %turn, ?phase, "backend silent for too long; aborting");
                        self.timed_out = true;
                        break Ok(TurnOutcome::Cancelled);
                    }
                    id = reported(&mut ids) => {
                        if saving.is_none() {
                            saving = Some(save_id(turns, record, id));
                        } else {
                            waiting = Some(id);
                        }
                    }
                    () = async { if let Some(write) = saving.as_mut() { write.await } }, if saving.is_some() => {
                        saving = waiting.take().map(|id| save_id(turns, record, id));
                    }
                }
            };
            let limit = self.timed_out.then_some(turns.config.timing.store_stall);
            finish_writes(turns, record, turn, (saving, waiting), limit).await;
            finished
        };
        if self.timed_out {
            // The event waits in the sink if the queue stays full; the turn's end sends it.
            let limit = self.turns.config.timing.store_stall;
            let sent = tokio::time::timeout(limit, self.interrupted(InterruptCause::IdleTimeout));
            if sent.await.is_err() {
                warn!(session = %session, turn = %turn, "the store did not answer; the interrupt is not sent yet");
            }
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
            self.emit
                .emit(Event::BackendCrashed {
                    session_id,
                    exit_code,
                    will_restart,
                    detail,
                })
                .await;
        }
        self.outcome = Some(result);
        if crashed && !replayable {
            warn!(session = %self.record.id, turn = %self.turn, "the backend crashed and the prompt cannot be replayed; failing the turn");
            self.fail_turn().await;
        }
        if !crashed || !replayable {
            return Step::Finished;
        }
        // The next run resumes the backend's conversation, and the old group must be gone first.
        let session = &self.record.id;
        self.note_conversation().await;
        stop(&mut self.child, session).await;
        remove_pidfile(&self.turns.config.pidfile_dir, session);
        self.crashed_cancel = Some(cancel);
        Step::Crashed
    }
}

impl<E: Emit> Attempt for Drive<'_, E> {
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
