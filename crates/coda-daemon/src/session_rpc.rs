// SPDX-License-Identifier: AGPL-3.0-only

//! The `session.create` and `session.resume` requests, backed by the store.
//!
//! Resume only confirms the session exists and returns it; the client then
//! replays what it missed with `session.subscribe` and its last sequence number.

use std::path::Path;
use std::sync::Arc;

use coda_protocol::jsonrpc::{ErrorObject, INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND};
use coda_protocol::methods::{
    SessionCreateParams, SessionCreateResult, SessionResumeParams, SessionResumeResult,
};
use coda_protocol::types::SessionInfo;
use coda_store::{NewSession, SessionRecord, StoreError};
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

#[cfg(all(test, unix))]
mod tests {
    use coda_protocol::jsonrpc::INVALID_PARAMS;
    use coda_store::Store;
    use serde_json::{Value, json};

    use super::create;
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
}
