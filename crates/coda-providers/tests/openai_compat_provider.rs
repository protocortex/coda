// SPDX-License-Identifier: AGPL-3.0-only

//! The openai-compat provider against a local mock server bound to port 0.
//! All streams are synthetic: hand-written, never recorded from a live server.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coda_providers::anthropic::Timing;
use coda_providers::anthropic::provider::IDLE_CHECK_INTERVAL;
use coda_providers::anthropic::request::{RequestSettings, ToolSchema};
use coda_providers::model::{
    Clock, ModelEvent, ModelProvider, ModelRequest, ProviderError, Sleeper, StopReason, Usage,
};
use coda_providers::openai_compat::classify_status;
use coda_providers::openai_compat::provider::{OpenAiCompatOptions, OpenAiCompatProvider};
use coda_providers::testing::contract::{Collected, Scenario, collect, run_contract};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const WAIT: Duration = Duration::from_secs(5);
const SECRET: &str = "sk-test-secret";
const KEY_ENV: &str = "CODA_TEST_KEY";
const ERROR_BODY_CAP: usize = 64 * 1024;
const FLOOD_BYTES: usize = 64 * 1024 * 1024;
const OVERSIZED_ARGUMENTS: usize = 1024 * 1024 + 1;
const IDLE: Duration = Duration::from_secs(10);

enum Reply {
    Http {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    },
    Flood {
        status: u16,
    },
    EchoKey,
    /// A stream of `body`; with `hang` the connection then stays open and silent.
    Sse {
        body: String,
        hang: bool,
    },
}

struct Captured {
    head: String,
    body: Value,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }
}

struct Mock {
    url: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    flooded: Arc<AtomicUsize>,
}

impl Mock {
    fn attempts(&self) -> usize {
        self.requests.lock().expect("lock").len()
    }
}

fn start(replies: Vec<Reply>) -> Mock {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let port = std_listener.local_addr().expect("addr").port();
    let listener = TcpListener::from_std(std_listener).expect("listener");
    let mock = Mock {
        url: format!("http://127.0.0.1:{port}"),
        requests: Arc::default(),
        flooded: Arc::default(),
    };
    let replies = Arc::new(Mutex::new(replies));
    let requests = Arc::clone(&mock.requests);
    let flooded = Arc::clone(&mock.flooded);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let reply = {
                let mut queue = replies.lock().expect("lock");
                (!queue.is_empty()).then(|| queue.remove(0))
            };
            let shared = (Arc::clone(&requests), Arc::clone(&flooded));
            tokio::spawn(serve(socket, reply, shared));
        }
    });
    mock
}

async fn read_request(socket: &mut TcpStream) -> Captured {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];
    let head_end = loop {
        let read = socket.read(&mut buffer).await.expect("read");
        raw.extend_from_slice(&buffer[..read]);
        if let Some(at) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        assert!(read > 0, "request ended inside its head");
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            line.to_lowercase()
                .strip_prefix("content-length:")?
                .trim()
                .parse()
                .ok()
        })
        .unwrap_or(0);
    while raw.len() < head_end + length {
        let read = socket.read(&mut buffer).await.expect("read");
        assert!(read > 0, "request ended inside its body");
        raw.extend_from_slice(&buffer[..read]);
    }
    let body = serde_json::from_slice(&raw[head_end..head_end + length]).unwrap_or(Value::Null);
    Captured { head, body }
}

async fn flood(socket: &mut TcpStream, status: u16, sent: &AtomicUsize) {
    let head = format!(
        "HTTP/1.1 {status} X\r\ncontent-length: {FLOOD_BYTES}\r\nconnection: close\r\n\r\n"
    );
    let _ = socket.write_all(head.as_bytes()).await;
    let chunk = vec![b'x'; 16 * 1024];
    while sent.load(Ordering::SeqCst) < FLOOD_BYTES {
        if socket.write_all(&chunk).await.is_err() {
            return;
        }
        sent.fetch_add(chunk.len(), Ordering::SeqCst);
    }
}

async fn write_http(socket: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &str) {
    let mut head = format!(
        "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    head.push_str("\r\n");
    socket.write_all(head.as_bytes()).await.expect("head");
    socket.write_all(body.as_bytes()).await.expect("body");
}

type Shared = (Arc<Mutex<Vec<Captured>>>, Arc<AtomicUsize>);

async fn serve(mut socket: TcpStream, reply: Option<Reply>, shared: Shared) {
    let (requests, flooded) = shared;
    let captured = read_request(&mut socket).await;
    let echoed = captured
        .header("authorization")
        .unwrap_or_default()
        .to_owned();
    requests.lock().expect("lock").push(captured);
    match reply {
        Some(Reply::EchoKey) => {
            let body = json!({"error": {"message": format!("bad credential {echoed}")}});
            write_http(&mut socket, 400, &[], &body.to_string()).await;
        }
        None => write_http(&mut socket, 418, &[], "script exhausted").await,
        Some(Reply::Http {
            status,
            headers,
            body,
        }) => write_http(&mut socket, status, &headers, &body).await,
        Some(Reply::Flood { status }) => flood(&mut socket, status, &flooded).await,
        Some(Reply::Sse { body, hang }) => {
            let head =
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
            socket.write_all(head.as_bytes()).await.expect("head");
            socket.write_all(body.as_bytes()).await.expect("body");
            socket.flush().await.expect("flush");
            let mut sink = [0u8; 64];
            while hang && socket.read(&mut sink).await.is_ok_and(|read| read > 0) {}
        }
    }
    let _ = socket.shutdown().await;
}

#[derive(Debug, Clone, Default)]
struct RecordingSleeper {
    delays: Arc<Mutex<Vec<Duration>>>,
}

impl RecordingSleeper {
    fn retry_delays(&self) -> Vec<Duration> {
        self.delays.lock().expect("lock").clone()
    }
}

impl Sleeper for RecordingSleeper {
    async fn sleep(&self, duration: Duration) {
        if duration != IDLE_CHECK_INTERVAL {
            self.delays.lock().expect("lock").push(duration);
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[derive(Debug, Clone)]
struct DrivenClock {
    base: Instant,
    offset_ms: Arc<AtomicU64>,
}

impl DrivenClock {
    fn advance(&self, by: Duration) {
        let millis = u64::try_from(by.as_millis()).expect("fits");
        self.offset_ms.fetch_add(millis, Ordering::SeqCst);
    }
}

impl Clock for DrivenClock {
    fn now(&self) -> Instant {
        self.base + Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
    }
}

fn frame(data: &Value) -> String {
    format!("data: {data}\n\n")
}

fn chunk(delta: &Value, finish: Option<&str>) -> String {
    frame(&json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}))
}

fn usage_chunk() -> String {
    frame(&json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2}}))
}

const DONE: &str = "data: [DONE]\n\n";

fn text_stream(pieces: &[&str]) -> String {
    let body: String = pieces
        .iter()
        .map(|piece| chunk(&json!({"content": piece}), None))
        .collect();
    body + &chunk(&json!({}), Some("stop")) + &usage_chunk() + DONE
}

fn call_delta(index: Option<u64>, id: Option<&str>, name: Option<&str>, arguments: &str) -> String {
    let function = json!({"name": name, "arguments": arguments});
    let call = json!({"index": index, "id": id, "function": function});
    chunk(&json!({"tool_calls": [call]}), None)
}

fn tool_stream() -> String {
    call_delta(Some(0), Some("c1"), Some("read"), "{\"path\":\"a.txt\"}")
        + &chunk(&json!({}), Some("tool_calls"))
        + &usage_chunk()
        + DONE
}

fn sse(text: &str) -> Reply {
    Reply::Sse {
        body: text.to_owned(),
        hang: false,
    }
}

fn status(code: u16) -> Reply {
    Reply::Http {
        status: code,
        headers: Vec::new(),
        body: json!({"error": {"type": "x", "message": "boom"}}).to_string(),
    }
}

fn options(base_url: String) -> OpenAiCompatOptions {
    OpenAiCompatOptions {
        base_url,
        settings: RequestSettings {
            model: "test-model".to_owned(),
            max_tokens: 64,
            system: "be brief".to_owned(),
            tools: vec![ToolSchema {
                name: "read".to_owned(),
                description: "reads".to_owned(),
                input_schema: json!({"type": "object"}),
            }],
        },
        timing: Timing {
            idle_timeout: IDLE,
            ..Timing::default()
        },
    }
}

type TestProvider = OpenAiCompatProvider<RecordingSleeper, DrivenClock>;

fn secret_env(name: &str) -> Option<String> {
    (name == KEY_ENV).then(|| SECRET.to_owned())
}

fn build(
    base_url: String,
    key_env: Option<&str>,
    env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
) -> Result<(TestProvider, RecordingSleeper, DrivenClock), ProviderError> {
    let sleeper = RecordingSleeper::default();
    let clock = DrivenClock {
        base: Instant::now(),
        offset_ms: Arc::default(),
    };
    let provider = OpenAiCompatProvider::new(
        options(base_url),
        key_env.map(str::to_owned),
        env,
        sleeper.clone(),
        clock.clone(),
    )?;
    Ok((provider, sleeper, clock))
}

fn provider(mock: &Mock) -> (TestProvider, RecordingSleeper, DrivenClock) {
    build(mock.url.clone(), Some(KEY_ENV), secret_env).expect("provider")
}

fn request() -> ModelRequest {
    ModelRequest::user_prompt("hello")
}

async fn run(mock: &Mock) -> (Collected, RecordingSleeper) {
    let (provider, sleeper, _clock) = provider(mock);
    (collect(&provider, request(), WAIT).await, sleeper)
}

fn text(piece: &str) -> ModelEvent {
    ModelEvent::TextDelta {
        text: piece.to_owned(),
    }
}

fn usage(input_tokens: u64, output_tokens: u64) -> ModelEvent {
    ModelEvent::Usage(Usage {
        input_tokens,
        output_tokens,
    })
}

fn stop(reason: StopReason) -> ModelEvent {
    ModelEvent::Stop { reason }
}

fn start_of(call_id: &str, name: &str) -> ModelEvent {
    ModelEvent::ToolCallStart {
        call_id: call_id.to_owned(),
        name: name.to_owned(),
    }
}

fn fragment(call_id: &str, piece: &str) -> ModelEvent {
    ModelEvent::ToolCallArgumentsDelta {
        call_id: call_id.to_owned(),
        fragment: piece.to_owned(),
    }
}

fn error_text(collected: &Collected) -> String {
    collected
        .error
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default()
}
fn tool_turn(calls: &[(&str, &str)]) -> Vec<ModelEvent> {
    let mut events: Vec<_> = calls
        .iter()
        .flat_map(|(id, name)| [start_of(id, name), fragment(id, "{}")])
        .collect();
    events.extend([usage(0, 0), stop(StopReason::ToolUse)]);
    events
}

#[tokio::test]
async fn streams_map_to_the_expected_events() {
    let finish = |reason| chunk(&json!({}), Some(reason));
    let cases: Vec<(&str, String, Vec<ModelEvent>)> = vec![
        (
            "text, then usage after the finish chunk, ended by [DONE]",
            text_stream(&["Hel", "lo"]),
            vec![
                text("Hel"),
                text("lo"),
                usage(3, 2),
                stop(StopReason::EndTurn),
            ],
        ),
        (
            "parallel tool calls are told apart by index",
            call_delta(Some(0), Some("a"), Some("read"), "")
                + &call_delta(Some(1), Some("b"), Some("write"), "")
                + &call_delta(Some(0), None, None, "{\"p\":")
                + &call_delta(Some(1), None, None, "{\"q\":1}")
                + &call_delta(Some(0), None, None, "1}")
                + &finish("tool_calls")
                + DONE,
            vec![
                start_of("a", "read"),
                start_of("b", "write"),
                fragment("a", "{\"p\":"),
                fragment("b", "{\"q\":1}"),
                fragment("a", "1}"),
                usage(0, 0),
                stop(StopReason::ToolUse),
            ],
        ),
        (
            "finish reason stop with tool calls is a tool use",
            call_delta(Some(0), Some("a"), Some("read"), "{}") + &finish("stop") + DONE,
            tool_turn(&[("a", "read")]),
        ),
        (
            "an id that differs, a missing index and an empty id each start a call",
            call_delta(Some(0), Some("a"), Some("read"), "{}")
                + &call_delta(Some(0), Some("b"), Some("write"), "{}")
                + &call_delta(None, Some("c"), Some("read"), "{}")
                + &call_delta(Some(1), Some(""), Some("write"), "{}")
                + &call_delta(Some(2), Some("call_4"), Some("read"), "{}")
                + &call_delta(Some(3), None, Some("write"), "{}")
                + &finish("tool_calls")
                + DONE,
            tool_turn(&[
                ("a", "read"),
                ("b", "write"),
                ("c", "read"),
                ("call_3", "write"),
                ("call_4", "read"),
                ("call_5", "write"),
            ]),
        ),
        (
            "the end of the stream after a finish reason completes the turn",
            chunk(&json!({"content": "hi"}), None) + &finish("stop"),
            vec![text("hi"), usage(0, 0), stop(StopReason::EndTurn)],
        ),
    ];
    for (name, body, expected) in cases {
        // Arrange
        let mock = start(vec![sse(&body)]);

        // Act
        let (collected, _sleeper) = run(&mock).await;

        // Assert
        assert_eq!(collected.error, None, "{name}");
        assert_eq!(collected.events, expected, "{name}");
    }
}

#[tokio::test]
async fn arguments_past_the_cap_end_the_stream_with_a_fatal_error() {
    // Arrange
    let oversized = "x".repeat(OVERSIZED_ARGUMENTS);
    let body = call_delta(Some(0), Some("a"), Some("read"), &oversized);
    let mock = start(vec![sse(&body)]);

    // Act
    let (collected, _sleeper) = run(&mock).await;

    // Assert
    assert!(matches!(collected.error, Some(ProviderError::Fatal { .. })));
    assert!(
        !collected
            .events
            .iter()
            .any(|event| matches!(event, ModelEvent::Stop { .. }))
    );
}

#[tokio::test]
async fn a_stream_that_ends_before_done_after_output_is_a_partial_turn() {
    // Arrange
    let partial = chunk(&json!({"content": "par"}), None);
    let mock = start(vec![sse(&partial), sse(&text_stream(&["again"]))]);

    // Act
    let (collected, sleeper) = run(&mock).await;

    // Assert
    assert_eq!(collected.events, [text("par")]);
    assert!(matches!(collected.error, Some(ProviderError::Fatal { .. })));
    assert!(error_text(&collected).contains("partial turn"));
    assert_eq!(mock.attempts(), 1);
    assert_eq!(sleeper.retry_delays(), Vec::<Duration>::new());
}

#[tokio::test]
async fn a_401_and_a_400_are_fatal_and_not_retried() {
    for code in [401, 400] {
        // Arrange
        let mock = start(vec![status(code), sse(&text_stream(&["ok"]))]);

        // Act
        let (collected, sleeper) = run(&mock).await;

        // Assert
        assert!(
            matches!(collected.error, Some(ProviderError::Fatal { .. })),
            "status {code}"
        );
        assert_eq!(mock.attempts(), 1, "status {code}");
        assert_eq!(sleeper.retry_delays(), Vec::<Duration>::new());
        assert!(!error_text(&collected).contains(SECRET));
    }
}

#[tokio::test]
async fn a_429_and_a_503_are_retried_through_the_sleeper() {
    // Arrange
    let mock = start(vec![
        Reply::Http {
            status: 429,
            headers: vec![("retry-after", "7".to_owned())],
            body: String::new(),
        },
        status(503),
        sse(&text_stream(&["ok"])),
    ]);
    let backoff = Timing::default().retry_delay(2, None, std::time::SystemTime::now());

    // Act
    let (collected, sleeper) = run(&mock).await;

    // Assert
    assert_eq!(collected.error, None);
    assert_eq!(mock.attempts(), 3);
    assert_eq!(sleeper.retry_delays(), [Duration::from_secs(7), backoff]);
}

#[tokio::test]
async fn a_stalled_stream_is_aborted_once_the_driven_clock_passes_the_idle_timeout() {
    // Arrange
    let first = chunk(&json!({"content": "a"}), None);
    let mock = start(vec![Reply::Sse {
        body: first,
        hang: true,
    }]);
    let (provider, _sleeper, clock) = provider(&mock);
    let mut stream = provider.stream(request()).await.expect("stream");
    let first_event = tokio::time::timeout(WAIT, stream.next())
        .await
        .expect("event");
    assert_eq!(first_event, Some(Ok(text("a"))));

    // Act
    clock.advance(IDLE + Duration::from_secs(1));
    let outcome = tokio::time::timeout(WAIT, stream.next())
        .await
        .expect("outcome");

    // Assert
    let error = outcome.expect("an item").expect_err("idle abort");
    assert!(error.to_string().contains("idle"), "{error}");
    assert_eq!(mock.attempts(), 1);
}

#[tokio::test]
async fn a_redirect_is_fatal_and_never_followed() {
    // Arrange
    let target = start(vec![sse(&text_stream(&["leaked"]))]);
    let location = format!("{}/chat/completions", target.url);
    let source = start(vec![Reply::Http {
        status: 307,
        headers: vec![("location", location)],
        body: String::new(),
    }]);

    // Act
    let (collected, sleeper) = run(&source).await;

    // Assert
    assert!(matches!(collected.error, Some(ProviderError::Fatal { .. })));
    assert_eq!(source.attempts(), 1);
    assert_eq!(target.attempts(), 0);
    assert_eq!(sleeper.retry_delays(), Vec::<Duration>::new());
}

#[tokio::test]
async fn an_error_body_is_read_only_up_to_the_cap() {
    // Arrange
    let mock = start(vec![Reply::Flood { status: 400 }]);

    // Act
    let (collected, _sleeper) = run(&mock).await;

    // Assert
    assert!(matches!(collected.error, Some(ProviderError::Fatal { .. })));
    let sent = mock.flooded.load(Ordering::SeqCst);
    assert!(sent >= ERROR_BODY_CAP, "{sent}");
    assert!(sent < FLOOD_BYTES / 4, "the server could send {sent} bytes");
}

#[tokio::test]
async fn the_request_is_a_streaming_chat_completion_with_bearer_auth_under_the_base_path() {
    // Arrange
    let mock = start(vec![sse(&text_stream(&["ok"]))]);
    let (provider, _sleeper, _clock) =
        build(format!("{}/v1/", mock.url), Some(KEY_ENV), secret_env).expect("provider");

    // Act
    let collected = collect(&provider, request(), WAIT).await;

    // Assert
    assert_eq!(collected.error, None);
    let requests = mock.requests.lock().expect("lock");
    let sent = &requests[0];
    assert!(sent.head.starts_with("POST /v1/chat/completions "));
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {SECRET}").as_str())
    );
    assert_eq!(sent.body["stream"], true);
    assert_eq!(sent.body["stream_options"]["include_usage"], true);
    assert_eq!(sent.body["messages"][0]["role"], "system");
    assert_eq!(sent.body["messages"][1]["content"], "hello");
    assert_eq!(sent.body["tools"][0]["type"], "function");
    assert_eq!(sent.body["tools"][0]["function"]["name"], "read");
}

#[tokio::test]
async fn no_authorization_header_is_sent_when_no_key_is_configured_or_set() {
    for key_env in [None, Some("UNSET_VAR")] {
        // Arrange
        let mock = start(vec![sse(&text_stream(&["a"]))]);
        let (provider, _sleeper, _clock) =
            build(mock.url.clone(), key_env, secret_env).expect("provider");

        // Act
        let collected = collect(&provider, request(), WAIT).await;

        // Assert
        assert_eq!(collected.error, None);
        let requests = mock.requests.lock().expect("lock");
        assert_eq!(requests[0].header("authorization"), None);
    }
}

#[tokio::test]
async fn the_key_never_appears_in_debug_or_error_output() {
    // Arrange
    let mock = start(vec![Reply::EchoKey]);
    let (good, _sleeper, _clock) = provider(&mock);
    let (bad, _sleeper, _clock) = build(mock.url.clone(), Some(KEY_ENV), |_| {
        Some(format!("{SECRET}\nsplit"))
    })
    .expect("provider");

    // Act
    let echoed = collect(&good, request(), WAIT).await;
    let invalid = collect(&bad, request(), WAIT).await;

    // Assert
    assert!(!format!("{good:?}").contains(SECRET));
    assert!(error_text(&echoed).contains("HTTP 400"));
    assert!(!error_text(&echoed).contains(SECRET));
    assert!(matches!(invalid.error, Some(ProviderError::Fatal { .. })));
    assert!(!error_text(&invalid).contains(SECRET));
    assert_eq!(mock.attempts(), 1);
}

#[test]
fn an_unsafe_base_url_is_refused_without_echoing_it() {
    for url in [
        "http://api.example.test",
        "https://user:hunter2@api.example.test",
        "ftp://api.example.test",
        "not a url",
        "https://api.example.test/v1?api-version=x",
        "https://api.example.test/v1#section",
    ] {
        // Act
        let result = build(url.to_owned(), None, |_| None);

        // Assert
        let error = result.err().unwrap_or_else(|| panic!("{url} accepted"));
        assert!(matches!(error, ProviderError::Fatal { .. }), "{url}");
        assert!(!error.to_string().contains("hunter2"));
    }
}

#[test]
fn overflow_is_read_from_markers_and_from_a_top_level_message() {
    for body in [
        json!({"error": {"message": "the context size was exceeded"}}),
        json!({"message": "Context size exceeded"}),
    ] {
        // Act
        let error = classify_status(400, &body.to_string());

        // Assert
        assert_eq!(error, ProviderError::ContextWindowExceeded, "{body}");
    }
}

fn scenario_mock(scenario: Scenario) -> Mock {
    let overflow = json!({"error": {"code": "context_length_exceeded",
        "message": "maximum context length exceeded"}})
    .to_string();
    start(match scenario {
        Scenario::Text | Scenario::DropCancels => vec![sse(&text_stream(&["Hel", "lo"]))],
        Scenario::ToolCall => vec![sse(&tool_stream())],
        Scenario::ContextWindowExceeded => vec![Reply::Http {
            status: 400,
            headers: Vec::new(),
            body: overflow,
        }],
        Scenario::Retryable => vec![status(503), status(503), status(503)],
        Scenario::Fatal => vec![status(401)],
    })
}

#[tokio::test]
async fn the_provider_passes_the_shared_contract_suite() {
    // Arrange
    let factory = |scenario| provider(&scenario_mock(scenario)).0;

    // Act
    let result = run_contract(factory, WAIT).await;

    // Assert
    assert_eq!(result, Ok(()));
}
