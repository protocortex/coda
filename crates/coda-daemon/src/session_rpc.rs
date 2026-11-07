// SPDX-License-Identifier: Apache-2.0

//! The `session.create` and `session.resume` requests, backed by the store.
//!
//! Resume only confirms the session exists and returns it; the client then
//! replays what it missed with `session.subscribe` and its last sequence number.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use coda_protocol::jsonrpc::{ErrorObject, INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND};
use coda_protocol::methods::{
    SessionCreateParams, SessionCreateResult, SessionListParams, SessionListResult,
    SessionResumeParams, SessionResumeResult, SessionStatus, SessionSummary, SessionWorktreeParams,
    SessionWorktreeResult, WORKTREE_REFUSED_CODE,
};
use coda_protocol::types::{SessionId, SessionInfo};
use coda_store::{
    NewSession, SessionFilter, SessionRecord, SessionSummary as StoredSummary, SessionWorktree,
    StoreError,
};
use serde_json::Value;
use tracing::{Instrument, error, warn};

use crate::dispatch::{HandlerResult, error, parse_params, to_value};
use crate::subscribe::{Hub, HubError};
use crate::trace::session_span;
use crate::turn::SESSION_NOT_FOUND;
use crate::worktree::{Worktrees, remove_made};

fn info(record: SessionRecord) -> SessionInfo {
    SessionInfo {
        id: record.id,
        backend: record.backend,
        model: record.model,
        cwd: record.cwd,
        mode: record.mode,
    }
}

fn unavailable() -> ErrorObject {
    error(METHOD_NOT_FOUND, "Method not found")
}

fn failed(e: HubError) -> ErrorObject {
    match e {
        HubError::Store(StoreError::SessionNotFound { .. }) => {
            error(SESSION_NOT_FOUND, "Session not found")
        }
        other => {
            error!(error = %other, "session request failed");
            error(INTERNAL_ERROR, "Internal error")
        }
    }
}

/// The canonical form of `cwd`, which must be an absolute path to a directory.
async fn canonical_cwd(cwd: &str) -> Result<String, ErrorObject> {
    let invalid = || {
        error(
            INVALID_PARAMS,
            "cwd must be an absolute path to a directory",
        )
    };
    if !Path::new(cwd).is_absolute() {
        return Err(invalid());
    }
    let path = cwd.to_owned();
    let resolved = tokio::task::spawn_blocking(move || {
        let canonical = std::fs::canonicalize(path).ok()?;
        canonical.is_dir().then_some(canonical)
    })
    .await;
    match resolved {
        Ok(Some(canonical)) => canonical.to_str().map(str::to_owned).ok_or_else(invalid),
        _ => Err(invalid()),
    }
}

/// Answers `session.create`. Each call makes a new session, so a retry whose
/// first answer was lost leaves an unused extra one.
pub(crate) async fn create(hub: Option<&Arc<Hub>>, params: Option<Value>) -> HandlerResult {
    let hub = hub.ok_or_else(unavailable)?;
    let params: SessionCreateParams = parse_params(params)?;
    let cwd = canonical_cwd(&params.cwd).await?;
    let new = NewSession {
        backend: params.backend,
        model: params.model,
        cwd,
        mode: params.mode,
    };
    let record = hub
        .with_store_mut(move |store| {
            let id = store.create_session(&new)?;
            store.load_session(&id)
        })
        .await
        .map_err(failed)?;
    let session = info(record);
    to_value(&SessionCreateResult { session })
}

/// Answers `session.resume`. A read-only lookup, so a retry is safe.
pub(crate) async fn resume(hub: Option<&Arc<Hub>>, params: Option<Value>) -> HandlerResult {
    let hub = hub.ok_or_else(unavailable)?;
    let params: SessionResumeParams = parse_params(params)?;
    let id = params.session_id;
    let record = hub
        .with_store({
            let id = id.clone();
            move |store| store.load_session(&id)
        })
        .instrument(session_span(&id))
        .await
        .map_err(failed)?;
    to_value(&SessionResumeResult {
        session: info(record),
    })
}

/// Answers `session.worktree`: makes a git worktree for a session that has not
/// started and makes it the session's working directory. The folder, the branch
/// and the setup command all come from the daemon, never from the request.
pub(crate) async fn worktree(
    hub: Option<&Arc<Hub>>,
    trees: Option<&Arc<Worktrees>>,
    params: Option<Value>,
) -> HandlerResult {
    let hub = hub.ok_or_else(unavailable)?;
    let trees = trees.ok_or_else(unavailable)?;
    let params: SessionWorktreeParams = parse_params(params)?;
    let id = params.session_id;
    let (record, existing) = hub
        .with_store({
            let id = id.clone();
            move |store| Ok((store.load_session(&id)?, store.session_worktree(&id)?))
        })
        .await
        .map_err(failed)?;
    if existing.is_some() {
        return Err(refused("the session already has a worktree"));
    }
    let made = blocking_worktree({
        let trees = Arc::clone(trees);
        let id = id.clone();
        move || trees.create(Path::new(&record.cwd), &id)
    })
    .await?;
    let recorded = hub
        .with_store_mut({
            let id = id.clone();
            let made = made.clone();
            move |store| {
                store.set_session_worktree(&id, &made)?;
                store.load_session(&id)
            }
        })
        .await;
    match recorded {
        Ok(record) => to_value(&SessionWorktreeResult {
            session: info(record),
            branch: made.branch,
        }),
        Err(e) => {
            let unrecorded = made.clone();
            if let Err(failure) = blocking_worktree(move || remove_made(&unrecorded)).await {
                error!(error = ?failure, "could not remove a worktree that was not recorded");
            }
            match e {
                HubError::Store(StoreError::WorktreeRefused { .. }) => {
                    Err(refused("the session already has a worktree or has started"))
                }
                other => Err(failed(other)),
            }
        }
    }
}

/// Puts back the checkout of a session whose worktree folder was removed, before
/// a request that needs it. The path comes from the stored record and is
/// checked against the managed folder, so a request cannot choose it.
pub(crate) async fn ensure_checkout(
    hub: Option<&Arc<Hub>>,
    trees: Option<&Arc<Worktrees>>,
    params: Option<&Value>,
) -> Result<(), ErrorObject> {
    let (Some(hub), Some(trees)) = (hub, trees) else {
        return Ok(());
    };
    let Some(id) = params
        .and_then(|p| p.get("session_id"))
        .and_then(Value::as_str)
        .map(SessionId::new)
    else {
        return Ok(());
    };
    let recorded: Option<SessionWorktree> = hub
        .with_store({
            let id = id.clone();
            move |store| store.session_worktree(&id)
        })
        .await
        .map_err(failed)?;
    let Some(recorded) = recorded else {
        return Ok(());
    };
    if PathBuf::from(&recorded.path).exists() {
        return Ok(());
    }
    let trees = Arc::clone(trees);
    blocking_worktree(move || trees.restore(&id, &recorded)).await
}

fn refused(message: &str) -> ErrorObject {
    error(WORKTREE_REFUSED_CODE, message)
}

async fn blocking_worktree<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, crate::worktree::WorktreeError> + Send + 'static,
) -> Result<T, ErrorObject> {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(done)) => Ok(done),
        Ok(Err(e)) => {
            warn!(error = %e, "worktree request failed");
            Err(refused(&e.to_string()))
        }
        Err(e) => {
            error!(error = %e, "worktree task failed");
            Err(error(INTERNAL_ERROR, "Internal error"))
        }
    }
}

fn summary(stored: StoredSummary, running: &impl Fn(&SessionId) -> bool) -> SessionSummary {
    let status = if running(&stored.record.id) {
        SessionStatus::Running
    } else {
        SessionStatus::Idle
    };
    SessionSummary {
        id: stored.record.id,
        backend: stored.record.backend,
        cwd: stored.record.cwd,
        created_at: stored.record.created_at,
        updated_at: stored.updated_at,
        status,
    }
}

/// Sessions listed when the caller names no limit.
const DEFAULT_LIST_LIMIT: u32 = 50;
/// Ceiling on a caller's limit, so one call cannot read the whole history.
const MAX_LIST_LIMIT: u32 = 200;

/// Answers `session.list` with metadata only, newest first and capped. A read-only lookup, so a retry
/// is safe. A cwd that no longer exists is matched as given.
pub(crate) async fn list(
    hub: Option<&Arc<Hub>>,
    running: impl Fn(&SessionId) -> bool,
    params: Option<Value>,
) -> HandlerResult {
    let hub = hub.ok_or_else(unavailable)?;
    let params: SessionListParams = parse_params(params)?;
    let cwd = match params.cwd {
        Some(cwd) => Some(canonical_cwd(&cwd).await.unwrap_or(cwd)),
        None => None,
    };
    let limit = params
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .min(MAX_LIST_LIMIT);
    let stored = hub
        .with_store(move |store| store.list_session_summaries(&SessionFilter { cwd }, limit))
        .await
        .map_err(failed)?;
    let sessions = stored.into_iter().map(|s| summary(s, &running)).collect();
    to_value(&SessionListResult { sessions })
}

#[cfg(all(test, unix))]
mod tests {
    use coda_protocol::jsonrpc::INVALID_PARAMS;
    use coda_store::Store;
    use serde_json::{Value, json};

    use coda_protocol::types::SessionId;

    use std::sync::Arc;

    use coda_protocol::methods::WORKTREE_REFUSED_CODE;

    use super::{create, ensure_checkout, list, worktree};
    use crate::subscribe::{DEFAULT_QUEUE_CAPACITY, Hub};
    use crate::worktree::Worktrees;

    fn params(cwd: &str) -> Value {
        json!({"backend": "claude_code", "cwd": cwd, "mode": "default"})
    }

    #[tokio::test]
    async fn an_unusable_cwd_is_invalid_params() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::new(
            Store::open(&dir.path().join("data")).unwrap(),
            DEFAULT_QUEUE_CAPACITY,
        );
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        let missing = dir.path().join("missing");
        let cases = [
            String::new(),
            "relative/dir".to_owned(),
            missing.display().to_string(),
            file.display().to_string(),
        ];

        for cwd in cases {
            // Act
            let error = create(Some(&hub), Some(params(&cwd))).await.unwrap_err();

            // Assert
            assert_eq!(error.code, INVALID_PARAMS, "cwd {cwd:?}");
        }
    }

    #[tokio::test]
    async fn a_symlinked_cwd_is_stored_as_its_real_directory() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::new(
            Store::open(&dir.path().join("data")).unwrap(),
            DEFAULT_QUEUE_CAPACITY,
        );
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // Act
        let result = create(Some(&hub), Some(params(&link.display().to_string())))
            .await
            .unwrap();

        // Assert
        let expected = real.canonicalize().unwrap();
        assert_eq!(
            result["session"]["cwd"],
            json!(expected.display().to_string())
        );
    }

    fn open_hub(dir: &tempfile::TempDir) -> std::sync::Arc<Hub> {
        Hub::new(
            Store::open(&dir.path().join("data")).unwrap(),
            DEFAULT_QUEUE_CAPACITY,
        )
    }

    #[tokio::test]
    async fn listing_reports_metadata_status_and_filters_by_cwd() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let hub = open_hub(&dir);
        let here = dir.path().canonicalize().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let mut ids = Vec::new();
        for cwd in [here.as_path(), elsewhere.path(), here.as_path()] {
            let made = create(Some(&hub), Some(params(&cwd.display().to_string())))
                .await
                .unwrap();
            ids.push(made["session"]["id"].as_str().unwrap().to_owned());
        }
        let running = SessionId::new(ids[2].clone());

        // Act
        let filtered = list(
            Some(&hub),
            |id| *id == running,
            Some(json!({"cwd": here.display().to_string()})),
        )
        .await
        .unwrap();
        let everything = list(Some(&hub), |_| false, Some(json!({}))).await.unwrap();

        // Assert
        let listed = filtered["sessions"].as_array().unwrap();
        let summary: Vec<_> = listed
            .iter()
            .map(|s| (s["id"].as_str().unwrap(), s["status"].as_str().unwrap()))
            .collect();
        assert_eq!(summary, [(&*ids[2], "running"), (&*ids[0], "idle")]);
        assert_eq!(listed[0]["backend"], json!("claude_code"));
        assert!(listed[0]["created_at"].is_i64() && listed[0]["updated_at"].is_i64());
        assert_eq!(everything["sessions"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn listing_caps_at_the_requested_limit() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let hub = open_hub(&dir);
        let cwd = dir.path().canonicalize().unwrap().display().to_string();
        for _ in 0..3 {
            create(Some(&hub), Some(params(&cwd))).await.unwrap();
        }

        // Act
        let two = list(Some(&hub), |_| false, Some(json!({"limit": 2})))
            .await
            .unwrap();
        let huge = list(Some(&hub), |_| false, Some(json!({"limit": u32::MAX})))
            .await
            .unwrap();

        // Assert
        assert_eq!(two["sessions"].as_array().unwrap().len(), 2);
        assert_eq!(huge["sessions"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn listing_without_a_store_is_method_not_found() {
        // Arrange / Act
        let error = list(None, |_| false, None).await.unwrap_err();

        // Assert
        assert_eq!(error.code, coda_protocol::jsonrpc::METHOD_NOT_FOUND);
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
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

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: std::path::PathBuf,
        hub: std::sync::Arc<Hub>,
        trees: Arc<Worktrees>,
    }

    fn repo_with_hub() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "first"]);
        let hub = open_hub(&dir);
        let trees = Arc::new(Worktrees::new(dir.path().join("trees"), Vec::new()));
        Fixture {
            repo: repo.canonicalize().unwrap(),
            _dir: dir,
            hub,
            trees,
        }
    }

    async fn new_session(f: &Fixture) -> String {
        let made = create(Some(&f.hub), Some(params(&f.repo.display().to_string())))
            .await
            .unwrap();
        made["session"]["id"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn a_session_moves_into_a_new_worktree_and_its_folder_changes() {
        // Arrange
        let f = repo_with_hub();
        let id = new_session(&f).await;

        // Act
        let moved = worktree(
            Some(&f.hub),
            Some(&f.trees),
            Some(json!({"session_id": id})),
        )
        .await
        .unwrap();

        // Assert
        let expected = f.trees.path_for(&SessionId::new(id.clone()));
        assert_eq!(
            moved["session"]["cwd"],
            json!(expected.display().to_string())
        );
        assert_eq!(moved["branch"], json!(format!("coda/{id}")));
        assert!(expected.join("a.txt").exists());
        let stored = f
            .hub
            .with_store({
                let id = SessionId::new(id);
                move |store| store.session_worktree(&id)
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.path, expected.display().to_string());
        assert_eq!(stored.repo, f.repo.display().to_string());
    }

    #[tokio::test]
    async fn a_session_takes_a_worktree_once_and_the_second_try_changes_nothing() {
        // Arrange
        let f = repo_with_hub();
        let id = new_session(&f).await;
        let body = || Some(json!({"session_id": id}));
        worktree(Some(&f.hub), Some(&f.trees), body())
            .await
            .unwrap();

        // Act
        let again = worktree(Some(&f.hub), Some(&f.trees), body())
            .await
            .unwrap_err();

        // Assert
        assert_eq!(again.code, WORKTREE_REFUSED_CODE);
        let branches = git(&f.repo, &["branch", "--format=%(refname:short)"]);
        assert_eq!(branches.lines().count(), 2, "{branches}");
    }

    #[tokio::test]
    async fn a_folder_that_is_not_a_repository_is_refused_and_the_session_is_unchanged() {
        // Arrange
        let f = repo_with_hub();
        let plain = tempfile::tempdir().unwrap();
        let made = create(
            Some(&f.hub),
            Some(params(
                &plain.path().canonicalize().unwrap().display().to_string(),
            )),
        )
        .await
        .unwrap();
        let id = made["session"]["id"].as_str().unwrap().to_owned();

        // Act
        let error = worktree(
            Some(&f.hub),
            Some(&f.trees),
            Some(json!({"session_id": id})),
        )
        .await
        .unwrap_err();

        // Assert
        assert_eq!(error.code, WORKTREE_REFUSED_CODE);
        let record = f
            .hub
            .with_store(move |store| store.load_session(&SessionId::new(id)))
            .await
            .unwrap();
        assert_eq!(
            record.cwd,
            plain.path().canonicalize().unwrap().display().to_string()
        );
    }

    #[tokio::test]
    async fn an_unknown_session_or_a_daemon_without_worktrees_is_an_error() {
        // Arrange
        let f = repo_with_hub();
        let id = new_session(&f).await;

        // Act
        let unknown = worktree(
            Some(&f.hub),
            Some(&f.trees),
            Some(json!({"session_id": "999"})),
        )
        .await
        .unwrap_err();
        let unavailable = worktree(Some(&f.hub), None, Some(json!({"session_id": id})))
            .await
            .unwrap_err();

        // Assert
        assert_eq!(unknown.code, crate::turn::SESSION_NOT_FOUND);
        assert_eq!(unavailable.code, coda_protocol::jsonrpc::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn a_removed_checkout_is_put_back_when_the_session_is_used_again() {
        // Arrange
        let f = repo_with_hub();
        let id = new_session(&f).await;
        let moved = worktree(
            Some(&f.hub),
            Some(&f.trees),
            Some(json!({"session_id": id})),
        )
        .await
        .unwrap();
        let path = std::path::PathBuf::from(moved["session"]["cwd"].as_str().unwrap());
        std::fs::remove_dir_all(&path).unwrap();

        // Act
        ensure_checkout(
            Some(&f.hub),
            Some(&f.trees),
            Some(&json!({"session_id": id})),
        )
        .await
        .unwrap();

        // Assert
        assert!(path.join("a.txt").exists());
    }

    #[tokio::test]
    async fn a_session_without_a_worktree_needs_no_checkout() {
        // Arrange
        let f = repo_with_hub();
        let id = new_session(&f).await;

        // Act
        let result = ensure_checkout(
            Some(&f.hub),
            Some(&f.trees),
            Some(&json!({"session_id": id})),
        )
        .await;

        // Assert
        assert!(result.is_ok());
        assert!(!f.trees.root().exists());
    }
}
