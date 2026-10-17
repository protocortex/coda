// SPDX-License-Identifier: AGPL-3.0-only

//! The `session.create` and `session.resume` requests, backed by the store.
//!
//! Resume only confirms the session exists and returns it; the client then
//! replays what it missed with `session.subscribe` and its last sequence number.

use std::path::Path;
use std::sync::Arc;

use coda_protocol::jsonrpc::{ErrorObject, INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND};
use coda_protocol::methods::{
    SessionCreateParams, SessionCreateResult, SessionListParams, SessionListResult,
    SessionResumeParams, SessionResumeResult, SessionStatus, SessionSummary,
};
use coda_protocol::types::{SessionId, SessionInfo};
use coda_store::{
    NewSession, SessionFilter, SessionRecord, SessionSummary as StoredSummary, StoreError,
};
use serde_json::Value;
use tracing::{Instrument, error};

use crate::dispatch::{HandlerResult, error, parse_params, to_value};
use crate::subscribe::{Hub, HubError};
use crate::trace::session_span;
use crate::turn::SESSION_NOT_FOUND;

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

    use super::{create, list};
    use crate::subscribe::{DEFAULT_QUEUE_CAPACITY, Hub};

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
}
