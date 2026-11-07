// SPDX-License-Identifier: Apache-2.0

//! The `checkpoints.prune` request: report and remove old checkpoint refs.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use coda_config::checkpoints::PruneConfig;
use coda_protocol::jsonrpc::{ErrorObject, INTERNAL_ERROR, METHOD_NOT_FOUND};
use coda_protocol::methods::{CheckpointsPruneParams, PRUNE_REFUSED_CODE};
use coda_protocol::types::SessionId;
use coda_store::{SessionFilter, Store};
use serde_json::Value;
use tracing::error;

use crate::checkpoint_prune::{Probes, Subject, prune};
use crate::dispatch::{HandlerResult, error, parse_params, to_value};
use crate::subscribe::Hub;

/// Answers `checkpoints.prune`: reports the old checkpoint refs of every
/// session and, with `apply` and `[checkpoints.prune] enabled`, removes them
/// and forgets them on the turn rows, so a rewind to such a turn says it has no
/// snapshot.
pub(crate) async fn prune_checkpoints(
    hub: Option<&Arc<Hub>>,
    policy: Option<&PruneConfig>,
    running: impl Fn(&SessionId) -> bool + Send + 'static,
    params: Option<Value>,
) -> HandlerResult {
    let unavailable = || error(METHOD_NOT_FOUND, "Method not found");
    let hub = hub.ok_or_else(unavailable)?;
    let policy = policy.ok_or_else(unavailable)?.clone();
    let params: CheckpointsPruneParams = parse_params(params)?;
    if params.apply && !policy.enabled {
        return Err(error(
            PRUNE_REFUSED_CODE,
            "checkpoint pruning is off; set `enabled = true` under [checkpoints.prune] to allow it",
        ));
    }
    let subjects = hub.with_store(subjects).await.map_err(|e| internal(&e))?;
    let result = tokio::task::spawn_blocking(move || {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
            });
        let probes = Probes {
            now_secs,
            running: &running,
        };
        prune(&subjects, &policy, &probes, params.apply)
    })
    .await
    .map_err(|e| {
        error!(error = %e, "checkpoint pruning task failed");
        error(INTERNAL_ERROR, "Internal error")
    })?;
    let removed: Vec<String> = result
        .sessions
        .iter()
        .flat_map(|session| &session.refs)
        .filter(|pruned| pruned.removed)
        .map(|pruned| pruned.reference.clone())
        .collect();
    if !removed.is_empty() {
        hub.with_store_mut(move |store| store.clear_checkpoint_refs(&removed))
            .await
            .map_err(|e| internal(&e))?;
    }
    to_value(&result)
}

/// Where each session's repository is: the repository of its worktree when it
/// has one, else its folder.
fn subjects(store: &Store) -> Result<Vec<Subject>, coda_store::StoreError> {
    let repos: HashMap<SessionId, String> = store
        .worktree_sessions()?
        .into_iter()
        .map(|row| (row.session, row.worktree.repo))
        .collect();
    let mut subjects = Vec::new();
    for record in store.list_sessions(&SessionFilter::default())? {
        let rolled_back_turns = store.rolled_back_turns(&record.id)?;
        let mut recorded = HashSet::new();
        let mut rolled_back = HashSet::new();
        for checkpoint in store.turn_checkpoints(&record.id)? {
            recorded.extend(checkpoint.before);
            if let Some(after) = checkpoint.after {
                if rolled_back_turns.contains(&checkpoint.turn) {
                    rolled_back.insert(after.clone());
                }
                recorded.insert(after);
            }
        }
        subjects.push(Subject {
            folder: PathBuf::from(repos.get(&record.id).unwrap_or(&record.cwd)),
            session: record.id,
            recorded,
            rolled_back,
        });
    }
    Ok(subjects)
}

fn internal(e: &crate::subscribe::HubError) -> ErrorObject {
    error!(error = %e, "checkpoint pruning failed");
    error(INTERNAL_ERROR, "Internal error")
}

#[cfg(all(test, unix))]
mod tests {
    use std::process::Command;

    use coda_protocol::types::{BackendKind, Mode};
    use coda_store::{NewSession, Store};
    use serde_json::json;

    use super::*;
    use crate::subscribe::DEFAULT_QUEUE_CAPACITY;

    fn run(dir: &std::path::Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn policy(enabled: bool) -> PruneConfig {
        PruneConfig {
            enabled,
            keep_days: 30,
            keep_turns: 1,
            rewind_days: 7,
        }
    }

    /// A repository with a session whose two turns each have a checkpoint ref.
    fn world() -> (tempfile::TempDir, Arc<Hub>, SessionId, [String; 2]) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-q", "-m", "first"]);
        let mut store = Store::open(&dir.path().join("data")).unwrap();
        let session = store
            .create_session(&NewSession {
                backend: BackendKind::ClaudeCode,
                model: None,
                cwd: repo.canonicalize().unwrap().display().to_string(),
                mode: Mode::Default,
            })
            .unwrap();
        let mut refs = Vec::new();
        for _ in 0..2 {
            let turn = store.begin_turn(&session).unwrap();
            let reference = crate::checkpoint::ref_name(session.as_str(), turn.as_str());
            run(&repo, &["update-ref", &reference, "HEAD"]);
            store
                .record_checkpoint_after(&session, &turn, &reference)
                .unwrap();
            refs.push(reference);
        }
        let hub = Hub::new(store, DEFAULT_QUEUE_CAPACITY);
        (dir, hub, session, refs.try_into().unwrap())
    }

    #[tokio::test]
    async fn applying_removes_the_old_ref_and_forgets_it_on_the_turn_row() {
        // Arrange
        let (dir, hub, session, refs) = world();

        // Act
        let result = prune_checkpoints(
            Some(&hub),
            Some(&policy(true)),
            |_| false,
            Some(json!({"apply": true})),
        )
        .await
        .unwrap();

        // Assert
        assert_eq!(result["sessions"][0]["refs"][0]["removed"], json!(true));
        let repo = dir.path().join("repo");
        let left = run(&repo, &["for-each-ref", "--format=%(refname)", "refs/coda"]);
        assert_eq!(left.trim(), refs[1]);
        let stored = hub
            .with_store(move |store| store.turn_checkpoints(&session))
            .await
            .unwrap();
        assert_eq!(stored[0].after, None);
        assert_eq!(stored[1].after.as_deref(), Some(refs[1].as_str()));
    }

    #[tokio::test]
    async fn applying_is_refused_while_pruning_is_off_but_the_report_is_allowed() {
        // Arrange
        let (dir, hub, _session, _refs) = world();

        // Act
        let refused = prune_checkpoints(
            Some(&hub),
            Some(&policy(false)),
            |_| false,
            Some(json!({"apply": true})),
        )
        .await
        .unwrap_err();
        let report =
            prune_checkpoints(Some(&hub), Some(&policy(false)), |_| false, Some(json!({})))
                .await
                .unwrap();

        // Assert
        assert_eq!(refused.code, PRUNE_REFUSED_CODE);
        assert_eq!(report["applied"], json!(false));
        assert_eq!(report["sessions"][0]["refs"].as_array().unwrap().len(), 1);
        let repo = dir.path().join("repo");
        let left = run(&repo, &["for-each-ref", "--format=%(refname)", "refs/coda"]);
        assert_eq!(left.lines().count(), 2);
    }
}
