// SPDX-License-Identifier: Apache-2.0

//! Hand-written payloads committed under `tests/golden`. Each must keep
//! deserializing, serialize back to the same JSON, and tolerate unknown fields.

use std::fs;
use std::path::PathBuf;

use coda_protocol::events::{Event, InterruptCause, ToolCallStatus};
use coda_protocol::methods::{
    AgentRunParams, AgentRunResult, Capabilities, CheckpointsPruneParams, CheckpointsPruneResult,
    DaemonBackendsParams, DaemonBackendsResult, DaemonShutdownParams, DaemonShutdownResult,
    DaemonStatusParams, DaemonStatusResult, GrantAddParams, GrantAddResult, InitializeParams,
    InitializeResult, PermissionRequestParams, PermissionRespondParams, PermissionRespondResult,
    PingParams, PingResult, SessionAttention, SessionCancelParams, SessionCancelResult,
    SessionCloseParams, SessionCloseResult, SessionCreateParams, SessionCreateResult,
    SessionDiffParams, SessionDiffResult, SessionListParams, SessionListResult, SessionQueueParams,
    SessionQueueResult, SessionResumeAtResetParams, SessionResumeAtResetResult,
    SessionResumeParams, SessionResumeResult, SessionRewindParams, SessionRewindResult,
    SessionSendParams, SessionSendResult, SessionSetModeParams, SessionSetModeResult,
    SessionSetModelParams, SessionSetModelResult, SessionShipParams, SessionShipResult,
    SessionSubscribeParams, SessionSubscribeResult, SessionWatchPrParams, SessionWatchPrResult,
    SessionWorktreeParams, SessionWorktreeResult, UsageSummaryParams, UsageSummaryResult,
    WorktreeCleanParams, WorktreeCleanResult,
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
    session_send_params_limits: "session.send.params.limits" => SessionSendParams,
    session_send_params_request: "session.send.params.request" => SessionSendParams,
    session_cancel_params_request: "session.cancel.params.request" => SessionCancelParams,
    session_create_params_request: "session.create.params.request" => SessionCreateParams,
    session_send_params_queue: "session.send.params.queue" => SessionSendParams,
    session_send_result: "session.send.result" => SessionSendResult,
    session_send_result_queued: "session.send.result.queued" => SessionSendResult,
    session_send_params_steer: "session.send.params.steer" => SessionSendParams,
    session_send_result_steered: "session.send.result.steered" => SessionSendResult,
    session_queue_params: "session.queue.params" => SessionQueueParams,
    session_queue_params_discard: "session.queue.params.discard" => SessionQueueParams,
    session_queue_result: "session.queue.result" => SessionQueueResult,
    session_resume_at_reset_params: "session.resume_at_reset.params" => SessionResumeAtResetParams,
    session_resume_at_reset_result: "session.resume_at_reset.result" => SessionResumeAtResetResult,
    agent_run_params: "agent.run.params" => AgentRunParams,
    agent_run_result: "agent.run.result" => AgentRunResult,
    session_cancel_params: "session.cancel.params" => SessionCancelParams,
    session_cancel_params_force: "session.cancel.params.force" => SessionCancelParams,
    session_cancel_result: "session.cancel.result" => SessionCancelResult,
    session_close_params: "session.close.params" => SessionCloseParams,
    session_close_result: "session.close.result" => SessionCloseResult,
    session_diff_params: "session.diff.params" => SessionDiffParams,
    session_diff_params_patch: "session.diff.params.patch" => SessionDiffParams,
    session_diff_result: "session.diff.result" => SessionDiffResult,
    session_diff_result_patch: "session.diff.result.patch" => SessionDiffResult,
    session_rewind_params: "session.rewind.params" => SessionRewindParams,
    session_rewind_params_base: "session.rewind.params.base" => SessionRewindParams,
    session_rewind_params_keep: "session.rewind.params.keep" => SessionRewindParams,
    session_rewind_result: "session.rewind.result" => SessionRewindResult,
    session_watch_pr_params: "session.watch_pr.params" => SessionWatchPrParams,
    session_watch_pr_result: "session.watch_pr.result" => SessionWatchPrResult,
    session_ship_params: "session.ship.params" => SessionShipParams,
    session_ship_result: "session.ship.result" => SessionShipResult,
    session_worktree_params: "session.worktree.params" => SessionWorktreeParams,
    session_worktree_result: "session.worktree.result" => SessionWorktreeResult,
    worktree_clean_params: "worktree.clean.params" => WorktreeCleanParams,
    worktree_clean_result: "worktree.clean.result" => WorktreeCleanResult,
    checkpoints_prune_params: "checkpoints.prune.params" => CheckpointsPruneParams,
    checkpoints_prune_result: "checkpoints.prune.result" => CheckpointsPruneResult,
    session_rewind_result_kept: "session.rewind.result.kept" => SessionRewindResult,
    session_subscribe_params: "session.subscribe.params" => SessionSubscribeParams,
    session_subscribe_result: "session.subscribe.result" => SessionSubscribeResult,
    session_list_params: "session.list.params" => SessionListParams,
    session_list_params_all: "session.list.params.all" => SessionListParams,
    session_list_result: "session.list.result" => SessionListResult,
    session_resume_params: "session.resume.params" => SessionResumeParams,
    session_resume_result: "session.resume.result" => SessionResumeResult,
    permission_request_params: "permission.request.params" => PermissionRequestParams,
    permission_request_params_minimal: "permission.request.params.minimal" => PermissionRequestParams,
    permission_respond_params: "permission.respond.params" => PermissionRespondParams,
    permission_respond_result: "permission.respond.result" => PermissionRespondResult,
    grant_add_params: "grant.add.params" => GrantAddParams,
    grant_add_result: "grant.add.result" => GrantAddResult,
    usage_summary_params: "usage.summary.params" => UsageSummaryParams,
    usage_summary_params_all_sessions: "usage.summary.params.all_sessions" => UsageSummaryParams,
    usage_summary_result: "usage.summary.result" => UsageSummaryResult,
    session_set_mode_params: "session.set_mode.params" => SessionSetModeParams,
    session_set_mode_result: "session.set_mode.result" => SessionSetModeResult,
    session_set_model_params: "session.set_model.params" => SessionSetModelParams,
    session_set_model_result: "session.set_model.result" => SessionSetModelResult,
    daemon_backends_params: "daemon.backends.params" => DaemonBackendsParams,
    daemon_backends_result: "daemon.backends.result" => DaemonBackendsResult,
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
    event_usage_context: "event.usage.context" => Event,
    event_approvals_unavailable: "event.approvals_unavailable" => Event,
    event_history_not_restored: "event.history_not_restored" => Event,
    event_stream_delta: "event.stream_delta" => Event,
    event_backend_crashed: "event.backend_crashed" => Event,
    event_backend_crashed_minimal: "event.backend_crashed.minimal" => Event,
    event_turn_interrupted: "event.turn_interrupted" => Event,
    event_turn_interrupted_minimal: "event.turn_interrupted.minimal" => Event,
    event_compaction: "event.compaction" => Event,
    event_rate_limit: "event.rate_limit" => Event,
    event_rate_limit_minimal: "event.rate_limit.minimal" => Event,
    event_usage_limit: "event.usage_limit" => Event,
    event_usage_limit_minimal: "event.usage_limit.minimal" => Event,
    event_account: "event.account" => Event,
    event_account_minimal: "event.account.minimal" => Event,
    event_tool_call: "event.tool_call" => Event,
    event_tool_call_minimal: "event.tool_call.minimal" => Event,
}

#[test]
fn absent_optional_fields_stay_absent_when_serialized() {
    // Arrange
    let params = SessionCreateParams {
        backend: BackendKind::Faux,
        cwd: "/work".to_owned(),
        model: None,
        mode: Mode::Default,
        client_request_id: None,
    };
    let crashed = Event::BackendCrashed {
        session_id: SessionId::new("s-1"),
        exit_code: None,
        will_restart: false,
        detail: None,
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
fn a_session_list_from_an_older_daemon_reads_as_waiting_for_nothing_and_untitled() {
    // Arrange
    let legacy = golden("session.list.result.legacy");

    // Act
    let result: SessionListResult = serde_json::from_value(legacy).unwrap();

    // Assert
    assert_eq!(result.sessions[0].attention, SessionAttention::None);
    assert_eq!(result.sessions[0].title, None);
}

#[test]
fn an_attention_state_from_a_newer_daemon_reads_as_unknown() {
    // Arrange
    let value = json!("failed");

    // Act
    let attention: SessionAttention = serde_json::from_value(value).unwrap();

    // Assert
    assert_eq!(attention, SessionAttention::Unknown);
}

#[test]
fn an_initialize_result_without_capabilities_reads_as_no_capabilities() {
    // Arrange
    let legacy = golden("initialize.result.legacy");

    // Act
    let result: InitializeResult = serde_json::from_value(legacy).unwrap();

    // Assert
    assert_eq!(result.capabilities, Capabilities::default());
    assert!(!result.capabilities.usage_details);
}

#[test]
fn an_event_type_from_a_newer_peer_reads_as_unknown() {
    // Arrange
    let raw = json!({"type": "event_added_later", "session_id": "s-1", "extra": {"n": 1}});

    // Act
    let event: Event = serde_json::from_value(raw).unwrap();

    // Assert
    assert_eq!(event, Event::Unknown);
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
    assert_eq!(result.limits, []);
}

#[test]
fn an_interrupt_without_a_cause_reads_as_causeless() {
    // Arrange
    let legacy = golden("event.turn_interrupted.minimal");

    // Act
    let event: Event = serde_json::from_value(legacy).unwrap();

    // Assert
    assert!(matches!(event, Event::TurnInterrupted { cause: None, .. }));
}

#[test]
fn an_interrupt_names_its_cause_in_snake_case() {
    // Arrange
    let stored = golden("event.turn_interrupted");

    // Act
    let event: Event = serde_json::from_value(stored).unwrap();

    // Assert
    assert!(matches!(
        event,
        Event::TurnInterrupted {
            cause: Some(InterruptCause::IdleTimeout),
            ..
        }
    ));
}

#[test]
fn a_status_from_a_daemon_without_the_provider_flag_reads_as_having_one() {
    // Arrange
    let legacy = golden("daemon.status.result.legacy");

    // Act
    let status: DaemonStatusResult = serde_json::from_value(legacy).unwrap();

    // Assert
    assert!(status.any_provider_enabled);
}

#[test]
fn an_interrupt_with_a_cause_from_a_newer_peer_still_decodes() {
    // Arrange
    let future = json!({
        "type": "turn_interrupted", "session_id": "s-1", "turn_id": "t-1",
        "cause": "cause_added_by_a_newer_peer",
    });

    // Act
    let event: Event = serde_json::from_value(future).unwrap();

    // Assert
    assert!(matches!(
        event,
        Event::TurnInterrupted {
            cause: Some(InterruptCause::Unknown),
            ..
        }
    ));
}

#[test]
fn a_tool_call_status_from_a_newer_peer_still_decodes() {
    // Arrange
    let future = json!({
        "type": "tool_call", "session_id": "s-1", "turn_id": "t-1", "id": "c-1",
        "name": "Bash", "status": "status_added_by_a_newer_peer",
    });

    // Act
    let event: Event = serde_json::from_value(future).unwrap();

    // Assert
    assert!(matches!(
        event,
        Event::ToolCall {
            status: ToolCallStatus::Unknown,
            ..
        }
    ));
}
