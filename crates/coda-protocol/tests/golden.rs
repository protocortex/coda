// SPDX-License-Identifier: AGPL-3.0-only

//! Hand-written payloads committed under `tests/golden`. Each must keep
//! deserializing, serialize back to the same JSON, and tolerate unknown fields.

use std::fs;
use std::path::PathBuf;

use coda_protocol::events::Event;
use coda_protocol::methods::{
    DaemonShutdownParams, DaemonShutdownResult, DaemonStatusParams, DaemonStatusResult,
    GrantAddParams, GrantAddResult, InitializeParams, InitializeResult, PermissionRequestParams,
    PermissionRespondParams, PermissionRespondResult, PingParams, PingResult, SessionCancelParams,
    SessionCancelResult, SessionCreateParams, SessionCreateResult, SessionListParams,
    SessionListResult, SessionResumeParams, SessionResumeResult, SessionSendParams,
    SessionSendResult, SessionSetModelParams, SessionSetModelResult, SessionSubscribeParams,
    SessionSubscribeResult, UsageSummaryParams, UsageSummaryResult,
};
use coda_protocol::types::{BackendKind, Mode, SessionId};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const UNKNOWN_FIELD: &str = "field_added_by_a_newer_peer";

fn golden(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"));
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn assert_stable<T: Serialize + DeserializeOwned>(name: &str) {
    // Arrange
    let expected = golden(name);

    // Act
    let parsed: T = serde_json::from_value(expected.clone())
        .unwrap_or_else(|e| panic!("{name} no longer deserializes: {e}"));
    let reserialized = serde_json::to_value(&parsed).unwrap();

    // Assert
    assert_eq!(reserialized, expected, "{name} serializes differently");
}

/// Adds the unknown field to every object, so nested objects are covered too.
fn inject_unknown_field(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.values_mut().for_each(inject_unknown_field);
            map.insert(UNKNOWN_FIELD.to_owned(), json!(1));
        }
        Value::Array(items) => items.iter_mut().for_each(inject_unknown_field),
        _ => {}
    }
}

fn assert_tolerates_unknown_fields<T: Serialize + DeserializeOwned>(name: &str) {
    // Arrange
    let mut payload = golden(name);
    inject_unknown_field(&mut payload);

    // Act
    let parsed = serde_json::from_value::<T>(payload);

    // Assert
    assert!(parsed.is_ok(), "{name} rejects unknown fields");
}

macro_rules! golden_tests {
    ($($test:ident: $name:literal => $ty:ty),+ $(,)?) => {
        $(
            #[test]
            fn $test() {
                assert_stable::<$ty>($name);
                assert_tolerates_unknown_fields::<$ty>($name);
            }
        )+
    };
}

golden_tests! {
    initialize_params: "initialize.params" => InitializeParams,
    initialize_result: "initialize.result" => InitializeResult,
    session_create_params: "session.create.params" => SessionCreateParams,
    session_create_params_minimal: "session.create.params.minimal" => SessionCreateParams,
    session_create_result: "session.create.result" => SessionCreateResult,
    session_send_params: "session.send.params" => SessionSendParams,
    session_send_result: "session.send.result" => SessionSendResult,
    session_cancel_params: "session.cancel.params" => SessionCancelParams,
    session_cancel_result: "session.cancel.result" => SessionCancelResult,
    session_subscribe_params: "session.subscribe.params" => SessionSubscribeParams,
    session_subscribe_result: "session.subscribe.result" => SessionSubscribeResult,
    session_list_params: "session.list.params" => SessionListParams,
    session_list_params_all: "session.list.params.all" => SessionListParams,
    session_list_result: "session.list.result" => SessionListResult,
    session_resume_params: "session.resume.params" => SessionResumeParams,
    session_resume_result: "session.resume.result" => SessionResumeResult,
    permission_request_params: "permission.request.params" => PermissionRequestParams,
    permission_respond_params: "permission.respond.params" => PermissionRespondParams,
    permission_respond_result: "permission.respond.result" => PermissionRespondResult,
    grant_add_params: "grant.add.params" => GrantAddParams,
    grant_add_result: "grant.add.result" => GrantAddResult,
    usage_summary_params: "usage.summary.params" => UsageSummaryParams,
    usage_summary_params_all_sessions: "usage.summary.params.all_sessions" => UsageSummaryParams,
    usage_summary_result: "usage.summary.result" => UsageSummaryResult,
    session_set_model_params: "session.set_model.params" => SessionSetModelParams,
    session_set_model_result: "session.set_model.result" => SessionSetModelResult,
    daemon_status_params: "daemon.status.params" => DaemonStatusParams,
    daemon_status_result: "daemon.status.result" => DaemonStatusResult,
    daemon_shutdown_params: "daemon.shutdown.params" => DaemonShutdownParams,
    daemon_shutdown_result: "daemon.shutdown.result" => DaemonShutdownResult,
    daemon_ping_params: "daemon.ping.params" => PingParams,
    daemon_ping_result: "daemon.ping.result" => PingResult,
    event_approve: "event.approve" => Event,
    event_deny: "event.deny" => Event,
    event_timeout: "event.timeout" => Event,
    event_result: "event.result" => Event,
    event_usage: "event.usage" => Event,
    event_approvals_unavailable: "event.approvals_unavailable" => Event,
    event_stream_delta: "event.stream_delta" => Event,
    event_backend_crashed: "event.backend_crashed" => Event,
    event_backend_crashed_minimal: "event.backend_crashed.minimal" => Event,
    event_turn_interrupted: "event.turn_interrupted" => Event,
}

#[test]
fn absent_optional_fields_stay_absent_when_serialized() {
    // Arrange
    let params = SessionCreateParams {
        backend: BackendKind::Faux,
        cwd: "/work".to_owned(),
        model: None,
        mode: Mode::Default,
    };
    let crashed = Event::BackendCrashed {
        session_id: SessionId::new("s-1"),
        exit_code: None,
    };

    // Act
    let params_json = serde_json::to_value(&params).unwrap();
    let crashed_json = serde_json::to_value(&crashed).unwrap();

    // Assert
    assert_eq!(
        params_json,
        json!({"backend": "faux", "cwd": "/work", "mode": "default"})
    );
    assert_eq!(
        crashed_json,
        json!({"type": "backend_crashed", "session_id": "s-1"})
    );
}

#[test]
fn usage_summary_result_without_savings_reads_as_zero_saved() {
    // Arrange
    let legacy = golden("usage.summary.result.legacy");

    // Act
    let result: UsageSummaryResult = serde_json::from_value(legacy).unwrap();

    // Assert
    assert_eq!(result.saved_bytes, 0);
    assert_eq!(result.usage.input_tokens, 100);
}
