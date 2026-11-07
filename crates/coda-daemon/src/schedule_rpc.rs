// SPDX-License-Identifier: Apache-2.0

//! The `schedule.add`, `schedule.list` and `schedule.remove` requests.

use std::sync::Arc;

use coda_protocol::jsonrpc::{ErrorObject, INTERNAL_ERROR, METHOD_NOT_FOUND};
use coda_protocol::methods::{
    SCHEDULE_REFUSED_CODE, ScheduleAddParams, ScheduleAddResult, ScheduleListResult,
    ScheduleRemoveParams, ScheduleRemoveResult,
};
use coda_store::StoreError;
use serde_json::Value;
use tracing::error;

use crate::dispatch::{HandlerResult, error, parse_params, to_value};
use crate::subscribe::{Hub, HubError};
use crate::turn::SESSION_NOT_FOUND;

fn failed(e: HubError) -> ErrorObject {
    match e {
        HubError::Store(StoreError::ScheduleRefused { reason }) => {
            error(SCHEDULE_REFUSED_CODE, &reason)
        }
        HubError::Store(StoreError::SessionNotFound { .. }) => {
            error(SESSION_NOT_FOUND, "Session not found")
        }
        other => {
            error!(error = %other, "schedule request failed");
            error(INTERNAL_ERROR, "Internal error")
        }
    }
}

fn hub_of(hub: Option<&Arc<Hub>>) -> Result<&Arc<Hub>, ErrorObject> {
    hub.ok_or_else(|| error(METHOD_NOT_FOUND, "Method not found"))
}

/// Answers `schedule.add`. The limits are the store's: a minute between runs at the least
/// and a fixed number of schedules.
pub(crate) async fn add(hub: Option<&Arc<Hub>>, params: Option<Value>) -> HandlerResult {
    let hub = hub_of(hub)?;
    let ScheduleAddParams { spec, target, text } = parse_params(params)?;
    let schedule = hub
        .with_store_mut(move |store| store.add_schedule(&spec, &target, &text))
        .await
        .map_err(failed)?;
    to_value(&ScheduleAddResult { schedule })
}

/// Answers `schedule.list`, a read-only lookup.
pub(crate) async fn list(hub: Option<&Arc<Hub>>) -> HandlerResult {
    let hub = hub_of(hub)?;
    let schedules = hub
        .with_store(coda_store::Store::list_schedules)
        .await
        .map_err(failed)?;
    to_value(&ScheduleListResult { schedules })
}

/// Answers `schedule.remove`.
pub(crate) async fn remove(hub: Option<&Arc<Hub>>, params: Option<Value>) -> HandlerResult {
    let hub = hub_of(hub)?;
    let ScheduleRemoveParams { id } = parse_params(params)?;
    let removed = hub
        .with_store_mut(move |store| store.remove_schedule(id))
        .await
        .map_err(failed)?;
    to_value(&ScheduleRemoveResult { removed })
}

#[cfg(test)]
mod tests {
    use coda_store::Store;
    use serde_json::json;

    use super::*;
    use crate::subscribe::DEFAULT_QUEUE_CAPACITY;

    fn hub() -> (tempfile::TempDir, Arc<Hub>, String) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("data")).unwrap();
        let id = store
            .create_session(&coda_store::NewSession {
                backend: coda_protocol::types::BackendKind::Faux,
                model: None,
                cwd: "/repo".to_owned(),
                mode: coda_protocol::types::Mode::Default,
            })
            .unwrap();
        (
            dir,
            Hub::new(store, DEFAULT_QUEUE_CAPACITY),
            id.as_str().to_owned(),
        )
    }

    fn body(session: &str, secs: u64) -> Value {
        json!({
            "spec": {"kind": "interval", "every_secs": secs},
            "target": {"kind": "session", "session_id": session},
            "text": "check the build"
        })
    }

    #[tokio::test]
    async fn an_added_schedule_is_listed_and_can_be_removed() {
        // Arrange
        let (_dir, hub, session) = hub();

        // Act
        let added = add(Some(&hub), Some(body(&session, 60))).await.unwrap();
        let listed = list(Some(&hub)).await.unwrap();
        let id = added["schedule"]["id"].as_u64().unwrap();
        let removed = remove(Some(&hub), Some(json!({"id": id}))).await.unwrap();
        let again = remove(Some(&hub), Some(json!({"id": id}))).await.unwrap();

        // Assert
        assert_eq!(listed["schedules"].as_array().unwrap().len(), 1);
        assert_eq!(removed["removed"], json!(true));
        assert_eq!(again["removed"], json!(false));
    }

    #[tokio::test]
    async fn an_interval_under_a_minute_is_refused_with_the_schedule_code() {
        // Arrange
        let (_dir, hub, session) = hub();

        // Act
        let refused = add(Some(&hub), Some(body(&session, 10))).await.unwrap_err();

        // Assert
        assert_eq!(refused.code, SCHEDULE_REFUSED_CODE);
    }

    #[tokio::test]
    async fn a_schedule_for_an_unknown_session_is_session_not_found() {
        // Arrange
        let (_dir, hub, _session) = hub();

        // Act
        let refused = add(Some(&hub), Some(body("999", 60))).await.unwrap_err();

        // Assert
        assert_eq!(refused.code, SESSION_NOT_FOUND);
    }

    #[tokio::test]
    async fn without_a_store_the_requests_are_method_not_found() {
        // Arrange and Act
        let refused = list(None).await.unwrap_err();

        // Assert
        assert_eq!(refused.code, METHOD_NOT_FOUND);
    }
}
