// SPDX-License-Identifier: Apache-2.0

//! `session.diff`: the files one turn changed, read from the checkpoints
//! around it.
//!
//! A turn's change runs from the end of the turn before it that is still part
//! of the conversation, or from the baseline before the first turn, to its own
//! end. Edits made between two turns count toward the later one, since nothing
//! is snapshotted at the start of a turn. Nothing in the repository changes.

use std::path::PathBuf;
use std::sync::Arc;

use coda_protocol::methods::{SessionDiffParams, SessionDiffResult};
use coda_protocol::types::TurnId;
use coda_store::TurnCheckpoint;

use super::{TurnError, Turns, not_found_or};
use crate::checkpoint::CheckpointError;
use crate::checkpoint_diff::turn_diff;

const UNKNOWN_TURN: &str = "that turn is not part of the session";
const ROLLED_BACK_TURN: &str = "that turn is rolled back";
const NO_TURN: &str = "no turn of this session has a snapshot of the files yet";
const NO_SNAPSHOT: &str = "that turn has no snapshot of the files";
const NO_BEFORE: &str = "there is no snapshot from before that turn to compare with";
const NOT_A_REPOSITORY: &str = "the folder is not a git repository, so there is no diff to show";
const DIFF_FAILED: &str = "could not read the snapshots of the files";

/// The two refs a turn's change lies between.
struct Span {
    turn: TurnId,
    before: String,
    after: String,
}

impl Turns {
    /// Reports the files `params.turn_id` changed, or the latest turn that has a
    /// snapshot, with the unified diff when `params.patch` asks for it.
    ///
    /// # Errors
    /// Returns [`TurnError::SessionNotFound`], [`TurnError::Busy`] while a turn
    /// runs, and [`TurnError::Diff`] when the turn has no snapshots to compare.
    pub async fn diff(
        self: &Arc<Self>,
        params: SessionDiffParams,
    ) -> Result<SessionDiffResult, TurnError> {
        let SessionDiffParams {
            session_id,
            turn_id,
            patch,
        } = params;
        let id = session_id.clone();
        let record = self
            .hub
            .with_store(move |store| store.load_session(&id))
            .await
            .map_err(|e| not_found_or(e, &session_id))?;
        self.await_wind_down(&session_id).await?;
        let id = session_id.clone();
        let (checkpoints, rolled_back) = self
            .hub
            .with_store(move |store| {
                Ok((store.turn_checkpoints(&id)?, store.rolled_back_turns(&id)?))
            })
            .await?;
        let live: Vec<&TurnCheckpoint> = checkpoints
            .iter()
            .filter(|c| !rolled_back.contains(&c.turn))
            .collect();
        if let Some(asked) = &turn_id {
            if !checkpoints.iter().any(|c| &c.turn == asked) {
                return Err(TurnError::Diff(UNKNOWN_TURN.to_owned()));
            }
            if rolled_back.contains(asked) {
                return Err(TurnError::Diff(ROLLED_BACK_TURN.to_owned()));
            }
        }
        let baseline = checkpoints.first().and_then(|c| c.before.as_deref());
        let span = span_of(&live, baseline, turn_id.as_ref())?;
        let folder = PathBuf::from(&record.cwd);
        let found = tokio::task::spawn_blocking(move || {
            turn_diff(&folder, &span.before, &span.after, patch).map(|found| (span.turn, found))
        })
        .await
        .map_err(|_| TurnError::Diff(DIFF_FAILED.to_owned()))?;
        let (turn, found) = found.map_err(|error| match error {
            CheckpointError::NotARepository => TurnError::Diff(NOT_A_REPOSITORY.to_owned()),
            other => {
                tracing::warn!(session = %session_id, error = %other, "could not read a turn diff");
                TurnError::Diff(DIFF_FAILED.to_owned())
            }
        })?;
        Ok(SessionDiffResult {
            turn_id: turn,
            files: found.files,
            patch: found.patch,
            truncated: found.truncated,
        })
    }
}

/// The turn to report and the refs around it. `live` holds the turns that are
/// not rolled back, oldest first, and `baseline` is the snapshot taken before
/// the first turn.
fn span_of(
    live: &[&TurnCheckpoint],
    baseline: Option<&str>,
    asked: Option<&TurnId>,
) -> Result<Span, TurnError> {
    let refused = |why: &str| TurnError::Diff(why.to_owned());
    let at = match asked {
        Some(turn) => live.iter().position(|c| &c.turn == turn),
        None => live.iter().rposition(|c| c.after.is_some()),
    }
    .ok_or_else(|| refused(NO_TURN))?;
    let after = live[at].after.clone().ok_or_else(|| refused(NO_SNAPSHOT))?;
    let before = live[..at]
        .iter()
        .rev()
        .find_map(|c| c.after.clone())
        .or_else(|| baseline.map(str::to_owned))
        .ok_or_else(|| refused(NO_BEFORE))?;
    Ok(Span {
        turn: live[at].turn.clone(),
        before,
        after,
    })
}
