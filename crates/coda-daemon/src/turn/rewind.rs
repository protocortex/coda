// SPDX-License-Identifier: Apache-2.0

//! `session.rewind`: puts the files back as an earlier turn left them and rolls
//! the conversation back to that turn.
//!
//! The files come from the turn's checkpoint, after the files as they are now
//! are saved under a ref of their own, so a restore never destroys work made
//! since the last turn. The conversation is not edited: the later turns are
//! marked rolled back and stay stored and shown, and the history a backend is
//! given leaves them out. A model-API session rebuilds its conversation from
//! the store on the next send. A Codex app-server thread and a Claude Code
//! conversation cannot be cut back, so the daemon drops them and the next turn
//! starts a new one told the kept turns as text.

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use coda_protocol::methods::{SessionRewindParams, SessionRewindResult};
use coda_protocol::types::{BackendKind, SessionId, TurnId};
use coda_store::SessionRecord;
use tracing::info;

use super::{TurnError, Turns, not_found_or, remove_pidfile_of, stop};
use crate::checkpoint::{self, CheckpointError, ref_name};

const UNKNOWN_TURN: &str = "that turn is not part of the session";
const ROLLED_BACK_TURN: &str = "that turn is already rolled back";
const NO_SNAPSHOT: &str =
    "that turn has no snapshot of the files; keep the files to rewind only the conversation";
const NO_BASELINE: &str = "the first turn has no snapshot from before it, so the files cannot go back to before it; name a turn instead";
const NOTHING_TO_DO: &str = "keeping both the files and the conversation leaves nothing to rewind";
const SHARED_FOLDER: &str =
    "another session with a running turn works in the same folder or one inside or above it";
const INDEX_LOCKED: &str = "another git process holds the index lock; try again when it ends";
const NOT_A_REPOSITORY: &str =
    "the folder is not a git repository, so there are no files to restore";
const SAVE_FAILED: &str = "could not save the current files first, so nothing was restored";
const PARTLY_RESTORED: &str = "the files may be partly restored; run the rewind again";
/// The label of the ref that holds the files as they were just before a restore.
const SAVED_LABEL: &str = "pre-rewind";

/// What a rewind goes back to, read before anything changes.
struct Target {
    /// The ref to restore the files from; absent when the turn has none.
    reference: Option<String>,
    /// How many turns the rewind newly rolls back.
    to_roll_back: usize,
}

impl Turns {
    /// Rewinds `session` to the end of `turn`, or to before its first turn when
    /// `turn` is `None`.
    ///
    /// # Errors
    /// Returns [`TurnError::SessionNotFound`], [`TurnError::Busy`] while a turn
    /// runs, and [`TurnError::Rewind`] when the rewind is refused. Nothing
    /// changes when it is refused.
    pub async fn rewind(
        self: &Arc<Self>,
        params: SessionRewindParams,
    ) -> Result<SessionRewindResult, TurnError> {
        let SessionRewindParams {
            session_id,
            turn_id,
            keep_files,
            keep_conversation,
        } = params;
        if keep_files && keep_conversation {
            return Err(TurnError::Rewind(NOTHING_TO_DO.to_owned()));
        }
        let id = session_id.clone();
        let record = self
            .hub
            .with_store(move |store| store.load_session(&id))
            .await
            .map_err(|e| not_found_or(e, &session_id))?;
        self.await_wind_down(&session_id).await?;
        let _hold = self.hold(&record)?;
        let target = self.rewind_target(&session_id, turn_id.as_ref()).await?;
        let mut saved_as = None;
        if !keep_files {
            let missing = if turn_id.is_some() {
                NO_SNAPSHOT
            } else {
                NO_BASELINE
            };
            let reference = target
                .reference
                .ok_or_else(|| TurnError::Rewind(missing.to_owned()))?;
            self.ensure_folder_is_free(&record).await?;
            saved_as = Some(save_current_files(&record).await?);
            restore_files(&record.cwd, &reference).await?;
        }
        let turns_rolled_back = if keep_conversation || target.to_roll_back == 0 {
            0
        } else {
            self.roll_back_conversation(&record, turn_id.as_ref())
                .await?
        };
        info!(session = %session_id, turns_rolled_back, files = !keep_files, "rewound the session");
        Ok(SessionRewindResult {
            files_restored: !keep_files,
            turns_rolled_back,
            saved_as,
        })
    }

    /// Keeps a send from starting while the rewind runs.
    pub(super) fn hold(
        self: &Arc<Self>,
        record: &SessionRecord,
    ) -> Result<Box<dyn Any + Send>, TurnError> {
        if record.backend.is_model_backend() {
            self.models.hold(&record.id)
        } else {
            Ok(Box::new(self.reserve(&record.id)?))
        }
    }

    async fn rewind_target(
        &self,
        session: &SessionId,
        turn: Option<&TurnId>,
    ) -> Result<Target, TurnError> {
        let id = session.clone();
        let (checkpoints, rolled_back) = self
            .hub
            .with_store(move |store| {
                Ok((store.turn_checkpoints(&id)?, store.rolled_back_turns(&id)?))
            })
            .await?;
        let refused = |why: &str| TurnError::Rewind(why.to_owned());
        let (reference, later) = match turn {
            Some(turn) => {
                let at = checkpoints
                    .iter()
                    .position(|c| &c.turn == turn)
                    .ok_or_else(|| refused(UNKNOWN_TURN))?;
                if rolled_back.contains(turn) {
                    return Err(refused(ROLLED_BACK_TURN));
                }
                (checkpoints[at].after.clone(), &checkpoints[at + 1..])
            }
            None => (
                checkpoints.first().and_then(|c| c.before.clone()),
                &checkpoints[..],
            ),
        };
        let to_roll_back = later
            .iter()
            .filter(|c| !rolled_back.contains(&c.turn))
            .count();
        Ok(Target {
            reference,
            to_roll_back,
        })
    }

    /// Refuses when a turn runs in another session whose folder is this one, or
    /// inside or above it: a restore would pull files from under it.
    async fn ensure_folder_is_free(&self, record: &SessionRecord) -> Result<(), TurnError> {
        let mut others: Vec<SessionId> = self.running().keys().cloned().collect();
        others.extend(self.models.running_sessions());
        others.retain(|id| id != &record.id);
        let mine = canonical(Path::new(&record.cwd));
        for other in others {
            let stored = self
                .hub
                .with_store(move |store| store.load_session(&other))
                .await;
            if let Ok(stored) = stored {
                let theirs = canonical(Path::new(&stored.cwd));
                if theirs.starts_with(&mine) || mine.starts_with(&theirs) {
                    return Err(TurnError::Rewind(SHARED_FOLDER.to_owned()));
                }
            }
        }
        Ok(())
    }

    /// Drops what the daemon holds of the conversation, then marks the later
    /// turns rolled back, so the next send builds it again from the kept turns.
    /// The backend's conversation id goes first: if it cannot be forgotten, no
    /// turn is marked, and the old conversation is never resumed with turns the
    /// user rolled back.
    async fn roll_back_conversation(
        &self,
        record: &SessionRecord,
        turn: Option<&TurnId>,
    ) -> Result<u64, TurnError> {
        if let Some(backend) = delegated(record.backend) {
            let id = record.id.clone();
            self.hub
                .with_store_mut(move |store| store.forget_backend_thread(&id, backend))
                .await?;
            self.forget_resume_id(record);
        }
        let (id, turn_id) = (record.id.clone(), turn.cloned());
        let marked = self
            .hub
            .with_store_mut(move |store| store.roll_back_turns_after(&id, turn_id.as_ref()))
            .await?;
        self.release(&record.id).await;
        Ok(marked)
    }

    /// Releases the pooled backend server and the idle model conversation, and
    /// returns whether either was held.
    pub(super) async fn release(&self, session: &SessionId) -> bool {
        let server = self.idle.take(session);
        let released = server.is_some();
        if let Some(mut server) = server {
            info!(session = %session, "closing the session's idle backend");
            let pid = server.pid();
            stop(&mut server, session).await;
            remove_pidfile_of(&self.config.pidfile_dir, session, pid);
        }
        released || self.models.close(session)
    }

    /// The earlier conversation as text for a Claude Code turn that starts a new
    /// conversation after a rewind or as the first turn of a fork; `None` when
    /// there is nothing to carry.
    pub(super) async fn rewind_seed(&self, record: &SessionRecord) -> Option<String> {
        if record.backend != BackendKind::ClaudeCode || self.resume_id(record).await.is_some() {
            return None;
        }
        let id = record.id.clone();
        let told = self
            .hub
            .with_store(move |store| {
                Ok(!store.rolled_back_turns(&id)?.is_empty() || store.fork_of(&id)?.is_some())
            })
            .await
            .unwrap_or(false);
        if !told {
            return None;
        }
        match self.carried(&record.id, None).await {
            coda_providers::codex::Carried::Seed(history) => Some(history),
            _ => None,
        }
    }
}

fn delegated(backend: BackendKind) -> Option<BackendKind> {
    (!backend.is_model_backend()).then_some(backend)
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Saves the files as they are now under a ref of their own and returns its name.
async fn save_current_files(record: &SessionRecord) -> Result<String, TurnError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    let reference = ref_name(record.id.as_str(), &format!("{SAVED_LABEL}-{millis}"));
    let (cwd, named) = (PathBuf::from(&record.cwd), reference.clone());
    let saved = tokio::task::spawn_blocking(move || checkpoint::snapshot(&cwd, &named))
        .await
        .map_err(|e| TurnError::Rewind(format!("{SAVE_FAILED}: {e}")))?;
    match saved {
        Ok(()) => Ok(reference),
        Err(CheckpointError::NotARepository) => Err(TurnError::Rewind(NOT_A_REPOSITORY.to_owned())),
        Err(other) => Err(TurnError::Rewind(format!("{SAVE_FAILED}: {other}"))),
    }
}

async fn restore_files(cwd: &str, reference: &str) -> Result<(), TurnError> {
    let (cwd, reference) = (PathBuf::from(cwd), reference.to_owned());
    let result = tokio::task::spawn_blocking(move || checkpoint::restore(&cwd, &reference))
        .await
        .map_err(|e| TurnError::Rewind(format!("{e}; {PARTLY_RESTORED}")))?;
    result.map_err(|error| match error {
        CheckpointError::IndexLocked => TurnError::Rewind(INDEX_LOCKED.to_owned()),
        CheckpointError::NotARepository => TurnError::Rewind(NOT_A_REPOSITORY.to_owned()),
        other => TurnError::Rewind(format!("{other}; {PARTLY_RESTORED}")),
    })
}
