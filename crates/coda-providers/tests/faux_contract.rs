// SPDX-License-Identifier: AGPL-3.0-only

//! The scripted faux provider and the shared provider contract suite.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use coda_config::config::Config;
use coda_providers::faux::{FauxError, FauxProvider, MAX_SCRIPT_BYTES, Turn};
use coda_providers::model::{
    ModelEvent, ModelProvider, ModelRequest, ModelStream, ProviderError, StopReason, Usage,
};
use coda_providers::testing::contract::{
    ActivityProbe, Scenario, collect, run_contract, run_drop_contract,
};

const SHORT_TIMEOUT: Duration = Duration::from_millis(200);

fn request() -> ModelRequest {
    ModelRequest {
        prompt: "hello".to_owned(),
    }
}

fn text(piece: &str) -> ModelEvent {
    ModelEvent::TextDelta {
        text: piece.to_owned(),
    }
}

fn stop(reason: StopReason) -> ModelEvent {
    ModelEvent::Stop { reason }
}

fn start(id: &str) -> ModelEvent {
    ModelEvent::ToolCallStart {
        call_id: id.to_owned(),
        name: "read".to_owned(),
    }
}

fn fragment(id: &str, piece: &str) -> ModelEvent {
    ModelEvent::ToolCallArgumentsDelta {
        call_id: id.to_owned(),
        fragment: piece.to_owned(),
    }
}

fn events_turn(events: Vec<ModelEvent>) -> Turn {
    Turn {
        events,
        error: None,
    }
}

fn failing_turn(error: ProviderError) -> Turn {
    Turn {
        events: Vec::new(),
        error: Some(error),
    }
}

fn conforming_turn(scenario: Scenario) -> Turn {
    match scenario {
        Scenario::Text => events_turn(vec![
            text("Hel"),
            text("lo"),
            ModelEvent::Usage(Usage {
                input_tokens: 3,
                output_tokens: 2,
            }),
            stop(StopReason::EndTurn),
        ]),
        Scenario::ToolCall => events_turn(vec![
            start("c1"),
            fragment("c1", "{\"path\":"),
            fragment("c1", "\"a.txt\"}"),
            usage(),
            stop(StopReason::ToolUse),
        ]),
        Scenario::ContextWindowExceeded => failing_turn(ProviderError::ContextWindowExceeded),
        Scenario::Retryable => failing_turn(ProviderError::Retryable {
            message: "overloaded".to_owned(),
        }),
        Scenario::Fatal => failing_turn(ProviderError::Fatal {
            message: "bad key".to_owned(),
        }),
        Scenario::DropCancels => unreachable!("run_contract does not run the drop scenario"),
    }
}

fn faux_for(scenario: Scenario) -> FauxProvider {
    FauxProvider::new(vec![conforming_turn(scenario)])
}

#[tokio::test]
async fn scripted_turn_streams_the_same_events_every_run() {
    // Arrange
    let turn = conforming_turn(Scenario::ToolCall);
    let expected = turn.events.clone();

    for _ in 0..2 {
        let provider = FauxProvider::new(vec![turn.clone()]);

        // Act
        let collected = collect(&provider, request(), SHORT_TIMEOUT).await;

        // Assert
        assert_eq!(collected.events, expected);
        assert_eq!(collected.error, None);
    }
}

#[tokio::test]
async fn each_stream_call_consumes_the_next_turn() {
    // Arrange
    let provider = FauxProvider::new(vec![
        events_turn(vec![text("one"), stop(StopReason::EndTurn)]),
        events_turn(vec![text("two"), stop(StopReason::EndTurn)]),
    ]);

    // Act
    let first = collect(&provider, request(), SHORT_TIMEOUT).await;
    let second = collect(&provider, request(), SHORT_TIMEOUT).await;

    // Assert
    assert_eq!(first.events[0], text("one"));
    assert_eq!(second.events[0], text("two"));
}

#[tokio::test]
async fn exhausted_script_is_a_fatal_error() {
    // Arrange
    let provider = FauxProvider::new(Vec::new());

    // Act
    let collected = collect(&provider, request(), SHORT_TIMEOUT).await;

    // Assert
    assert!(matches!(collected.error, Some(ProviderError::Fatal { .. })));
}

#[tokio::test]
async fn script_file_named_in_config_is_replayed() {
    // Arrange
    let dir = tempfile::tempdir().expect("temp dir");
    let script = dir.path().join("s.json");
    std::fs::write(
        &script,
        r#"[
          {"events":[
            {"type":"text_delta","text":"hi"},
            {"type":"usage","input_tokens":1,"output_tokens":1},
            {"type":"stop","reason":"end_turn"}]},
          {"events":[{"type":"text_delta","text":"x"}],
           "error":{"class":"retryable","message":"busy"}}
        ]"#,
    )
    .expect("write script");
    let config = format!(
        "[providers.faux]\nenabled = true\nscript = \"{}\"\n",
        script.display()
    );
    let parsed = Config::from_toml_str(&config).expect("config");

    // Act
    let provider = FauxProvider::from_config(&parsed.config).expect("faux provider");
    let first = collect(&provider, request(), SHORT_TIMEOUT).await;
    let second = collect(&provider, request(), SHORT_TIMEOUT).await;

    // Assert
    assert_eq!(first.events.len(), 3);
    assert_eq!(
        second.error,
        Some(ProviderError::Retryable {
            message: "busy".to_owned()
        })
    );
}

#[test]
fn config_without_a_script_is_rejected() {
    // Arrange
    let parsed = Config::from_toml_str("[providers.faux]\nenabled = true\n").expect("config");

    // Act
    let result = FauxProvider::from_config(&parsed.config);

    // Assert
    assert!(matches!(result, Err(FauxError::NoScript)));
}

#[test]
fn malformed_script_is_rejected_with_its_path() {
    // Arrange
    let dir = tempfile::tempdir().expect("temp dir");
    let script = dir.path().join("s.json");
    std::fs::write(&script, "not json").expect("write script");

    // Act
    let result = FauxProvider::from_script_file(&script);

    // Assert
    assert!(matches!(result, Err(FauxError::Parse { .. })));
}

#[tokio::test]
async fn contract_suite_passes_for_the_faux_provider() {
    // Act
    let result = run_contract(faux_for, SHORT_TIMEOUT).await;

    // Assert
    assert_eq!(result, Ok(()));
}

fn usage() -> ModelEvent {
    ModelEvent::Usage(Usage {
        input_tokens: 3,
        output_tokens: 2,
    })
}

/// A provider that violates one rule of the contract on purpose, with the
/// reason the suite must report.
fn broken_turn(case: &str) -> (Turn, &'static str) {
    match case {
        "event_after_stop" => (
            events_turn(vec![
                text("a"),
                usage(),
                stop(StopReason::EndTurn),
                text("late"),
            ]),
            "events were sent after the stop event",
        ),
        "missing_stop" => (
            events_turn(vec![text("a"), usage()]),
            "ended without a stop event",
        ),
        "no_usage" => (
            events_turn(vec![text("a"), stop(StopReason::EndTurn)]),
            "no usage event",
        ),
        "two_usage" => (
            events_turn(vec![text("a"), usage(), usage(), stop(StopReason::EndTurn)]),
            "more than one usage event",
        ),
        "arguments_before_start" => (
            events_turn(vec![
                fragment("c1", "{}"),
                start("c1"),
                usage(),
                stop(StopReason::ToolUse),
            ]),
            "arguments for c1 arrived before its start",
        ),
        "invalid_argument_json" => (
            events_turn(vec![
                start("c1"),
                fragment("c1", "{\"a\":"),
                fragment("c1", "1"),
                usage(),
                stop(StopReason::ToolUse),
            ]),
            "tool call c1 arguments are not valid JSON",
        ),
        "context_window_as_fatal" => (
            failing_turn(ProviderError::Fatal {
                message: "too long".to_owned(),
            }),
            "wrong error class: fatal provider error: too long",
        ),
        "fatal_as_retryable" => (
            failing_turn(ProviderError::Retryable {
                message: "bad key".to_owned(),
            }),
            "wrong error class: retryable provider error: bad key",
        ),
        other => panic!("unknown broken case {other}"),
    }
}

fn broken_scenario(case: &str) -> Scenario {
    match case {
        "event_after_stop" | "missing_stop" | "no_usage" | "two_usage" => Scenario::Text,
        "arguments_before_start" | "invalid_argument_json" => Scenario::ToolCall,
        "context_window_as_fatal" => Scenario::ContextWindowExceeded,
        _ => Scenario::Fatal,
    }
}

#[tokio::test]
async fn contract_suite_fails_for_each_deliberately_broken_provider() {
    let cases = [
        "event_after_stop",
        "missing_stop",
        "no_usage",
        "two_usage",
        "arguments_before_start",
        "invalid_argument_json",
        "context_window_as_fatal",
        "fatal_as_retryable",
    ];
    for case in cases {
        // Arrange
        let broken = broken_scenario(case);
        let (turn, expected_reason) = broken_turn(case);
        let factory = |scenario: Scenario| {
            if scenario == broken {
                FauxProvider::new(vec![turn.clone()])
            } else {
                faux_for(scenario)
            }
        };

        // Act
        let result = run_contract(factory, SHORT_TIMEOUT).await;

        // Assert
        let violation = result.expect_err(case);
        assert_eq!(violation.scenario, broken, "case {case}");
        assert!(
            violation.reason.contains(expected_reason),
            "case {case}: got reason {:?}",
            violation.reason
        );
    }
}

#[tokio::test]
async fn tool_calls_with_valid_joined_json_pass_however_they_are_fragmented() {
    let variants = [
        vec![fragment("c1", "{}")],
        vec![fragment("c1", ""), fragment("c1", "{}")],
        vec![fragment("c1", "{\"a\":"), fragment("c1", "1}")],
    ];
    for fragments in variants {
        // Arrange
        let mut events = vec![start("c1")];
        events.extend(fragments);
        events.extend([usage(), stop(StopReason::ToolUse)]);
        let factory = |scenario: Scenario| {
            if scenario == Scenario::ToolCall {
                FauxProvider::new(vec![events_turn(events.clone())])
            } else {
                faux_for(scenario)
            }
        };

        // Act
        let result = run_contract(factory, SHORT_TIMEOUT).await;

        // Assert
        assert_eq!(result, Ok(()), "{events:?}");
    }
}

/// Opens a stream and never sends or closes it.
struct HangingProvider {
    held: tokio::sync::Mutex<Vec<tokio::sync::mpsc::Sender<Result<ModelEvent, ProviderError>>>>,
}

impl ModelProvider for HangingProvider {
    async fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ProviderError> {
        let (sender, stream) = ModelStream::channel(1);
        self.held.lock().await.push(sender);
        Ok(stream)
    }
}

#[tokio::test]
async fn contract_suite_fails_for_a_stream_that_never_ends() {
    // Arrange
    let factory = |_scenario: Scenario| HangingProvider {
        held: tokio::sync::Mutex::new(Vec::new()),
    };

    // Act
    let result = run_contract(factory, SHORT_TIMEOUT).await;

    // Assert
    let violation = result.expect_err("the hang must be reported");
    assert!(violation.reason.contains("neither ended nor failed"));
}

#[test]
fn script_file_over_the_size_cap_is_rejected() {
    // Arrange
    let dir = tempfile::tempdir().expect("temp dir");
    let script = dir.path().join("s.json");
    let padding = " ".repeat(usize::try_from(MAX_SCRIPT_BYTES).expect("cap fits") + 1);
    std::fs::write(&script, padding).expect("write script");

    // Act
    let result = FauxProvider::from_script_file(&script);

    // Assert
    assert!(matches!(result, Err(FauxError::TooLarge { .. })));
}

#[test]
fn script_with_an_unknown_field_is_rejected_at_every_level() {
    let scripts = [
        r#"[{"events":[],"extra":1}]"#,
        r#"[{"events":[{"type":"text_delta","text":"a","extra":1}]}]"#,
        r#"[{"events":[{"type":"usage","input_tokens":1,"output_tokens":1,"extra":1}]}]"#,
        r#"[{"events":[],"error":{"class":"retryable","message":"m","extra":1}}]"#,
    ];
    for script in scripts {
        // Arrange
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("s.json");
        std::fs::write(&path, script).expect("write script");

        // Act
        let result = FauxProvider::from_script_file(&path);

        // Assert
        assert!(
            matches!(result, Err(FauxError::Parse { .. })),
            "accepted {script}"
        );
    }
}

/// Streams one event, then keeps "working" while holding an activity token.
struct WorkingProvider {
    probe: Arc<ActivityProbe>,
    cancels_on_drop: bool,
}

impl ModelProvider for WorkingProvider {
    fn stream(
        &self,
        _request: ModelRequest,
    ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send {
        let token = self.probe.token();
        let work = move |sender: tokio::sync::mpsc::Sender<_>| async move {
            let _ = sender.send(Ok(text("tick"))).await;
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                std::hint::black_box(&token);
            }
        };
        if self.cancels_on_drop {
            return std::future::ready(Ok(ModelStream::spawn(1, work)));
        }
        let (sender, stream) = ModelStream::channel(1);
        tokio::spawn(work(sender));
        std::future::ready(Ok(stream))
    }
}

#[tokio::test]
async fn drop_contract_passes_when_dropping_the_stream_stops_the_work() {
    // Act
    let result = run_drop_contract(
        |probe| WorkingProvider {
            probe,
            cancels_on_drop: true,
        },
        SHORT_TIMEOUT,
    )
    .await;

    // Assert
    assert_eq!(result, Ok(()));
}

#[tokio::test]
async fn drop_contract_fails_when_the_work_outlives_the_stream() {
    // Act
    let result = run_drop_contract(
        |probe| WorkingProvider {
            probe,
            cancels_on_drop: false,
        },
        SHORT_TIMEOUT,
    )
    .await;

    // Assert
    let violation = result.expect_err("the leak must be reported");
    assert_eq!(violation.scenario, Scenario::DropCancels);
    assert!(violation.reason.contains("kept running"));
}
