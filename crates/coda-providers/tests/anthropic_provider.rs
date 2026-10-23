// SPDX-License-Identifier: AGPL-3.0-only

//! The Anthropic provider against a local mock server bound to port 0. All
//! streams are synthetic.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coda_config::config::{Config, ProviderConfig};
use coda_protocol::types::BackendKind;
use coda_providers::anthropic::Timing;
use coda_providers::anthropic::provider::{
    AnthropicOptions, AnthropicProvider, BaseUrlError, IDLE_CHECK_INTERVAL, validate_base_url,
};
use coda_providers::anthropic::request::{RequestSettings, ToolSchema};
use coda_providers::model::{
    Clock, ModelEvent, ModelProvider, ModelRequest, ProviderError, Sleeper, StopReason, Usage,
};
use coda_providers::registry::ModelRegistry;
use coda_providers::testing::contract::{Collected, Scenario, collect, run_contract};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(5);
const SECRET: &str = "sk-ant-test-secret";
const KEY_ENV: &str = "CODA_TEST_KEY";
/// Mirrors the provider cap on how much of an error body is read.
const ERROR_BODY_CAP: usize = 64 * 1024;
const FLOOD_BYTES: usize = 64 * 1024 * 1024;
const IDLE: Duration = Duration::from_secs(10);

// ---- mock server ----------------------------------------------------------

enum Step {
    Bytes(Vec<u8>),
    Wait(Arc<Notify>),
    Hang,
}

enum Reply {
    Http {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    },
    /// An error reply whose body is far larger than any sane reader takes.
    Flood {
        status: u16,
    },
    Sse(Vec<Step>),
}

struct Captured {
    head: String,
    body: Value,
}

impl Captured {
    /// The value of a request header, found by its lowercase name.
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
    closed: Arc<Notify>,
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
        closed: Arc::default(),
        flooded: Arc::default(),
    };
    let replies = Arc::new(Mutex::new(replies.into_iter().collect::<Vec<_>>()));
    let (requests, closed) = (Arc::clone(&mock.requests), Arc::clone(&mock.closed));
    let flooded = Arc::clone(&mock.flooded);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let reply = {
                let mut queue = replies.lock().expect("lock");
                (!queue.is_empty()).then(|| queue.remove(0))
            };
            tokio::spawn(serve(
                socket,
                reply,
                Arc::clone(&requests),
                Arc::clone(&closed),
                Arc::clone(&flooded),
            ));
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

async fn serve(
    mut socket: TcpStream,
    reply: Option<Reply>,
    requests: Arc<Mutex<Vec<Captured>>>,
    closed: Arc<Notify>,
    flooded: Arc<AtomicUsize>,
) {
    let captured = read_request(&mut socket).await;
    requests.lock().expect("lock").push(captured);
    match reply {
        None => write_http(&mut socket, 418, &[], "script exhausted").await,
        Some(Reply::Http {
            status,
            headers,
            body,
        }) => write_http(&mut socket, status, &headers, &body).await,
        Some(Reply::Flood { status }) => flood(&mut socket, status, &flooded).await,
        Some(Reply::Sse(steps)) => {
            let head =
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
            socket.write_all(head.as_bytes()).await.expect("head");
            for step in steps {
                match step {
                    Step::Bytes(bytes) => {
                        socket.write_all(&bytes).await.expect("chunk");
                        socket.flush().await.expect("flush");
                    }
                    Step::Wait(gate) => gate.notified().await,
                    Step::Hang => {
                        let mut sink = [0u8; 64];
                        while socket.read(&mut sink).await.is_ok_and(|read| read > 0) {}
                        closed.notify_one();
                        return;
                    }
                }
            }
        }
    }
    let _ = socket.shutdown().await;
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

// ---- injected time --------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct RecordingSleeper {
    delays: Arc<Mutex<Vec<Duration>>>,
    ticks: Arc<Mutex<Vec<Duration>>>,
}

impl RecordingSleeper {
    fn retry_delays(&self) -> Vec<Duration> {
        self.delays.lock().expect("lock").clone()
    }
}

impl Sleeper for RecordingSleeper {
    async fn sleep(&self, duration: Duration) {
        let list = if duration == IDLE_CHECK_INTERVAL {
            &self.ticks
        } else {
            &self.delays
        };
        list.lock().expect("lock").push(duration);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[derive(Debug, Clone)]
struct DrivenClock {
    base: Instant,
    offset_ms: Arc<AtomicU64>,
}

impl DrivenClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            offset_ms: Arc::default(),
        }
    }

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

// ---- fixtures -------------------------------------------------------------

fn frame(data: &Value) -> String {
    format!("event: x\ndata: {data}\n\n")
}

fn message_start() -> String {
    frame(&json!({"type": "message_start", "message": {"usage": {"input_tokens": 3}}}))
}

fn text_block(piece: &str) -> String {
    frame(&json!({"type": "content_block_delta", "index": 0,
        "delta": {"type": "text_delta", "text": piece}}))
}

fn end(reason: &str) -> String {
    frame(
        &json!({"type": "message_delta", "delta": {"stop_reason": reason},
        "usage": {"output_tokens": 2}}),
    ) + &frame(&json!({"type": "message_stop"}))
}

fn text_stream(pieces: &[&str]) -> String {
    let body: String = pieces.iter().map(|piece| text_block(piece)).collect();
    message_start() + &body + &end("end_turn")
}

fn tool_stream() -> String {
    let start = frame(&json!({"type": "content_block_start", "index": 0,
        "content_block": {"type": "tool_use", "id": "c1", "name": "read"}}));
    let fragment = |piece: &str| {
        frame(&json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": piece}}))
    };
    let stop = frame(&json!({"type": "content_block_stop", "index": 0}));
    message_start()
        + &start
        + &fragment("{\"path\":")
        + &fragment("\"a.txt\"}")
        + &stop
        + &end("tool_use")
}

fn sse(text: &str) -> Reply {
    Reply::Sse(vec![Step::Bytes(text.as_bytes().to_vec())])
}

fn status(code: u16) -> Reply {
    Reply::Http {
        status: code,
        headers: Vec::new(),
        body: json!({"error": {"type": "x", "message": "boom"}}).to_string(),
    }
}

fn settings() -> RequestSettings {
    RequestSettings {
        model: "test-model".to_owned(),
        max_tokens: 64,
        system: "be brief".to_owned(),
        tools: vec![
            ToolSchema {
                name: "write".to_owned(),
                description: "writes".to_owned(),
                input_schema: json!({"type": "object"}),
            },
            ToolSchema {
                name: "read".to_owned(),
                description: "reads".to_owned(),
                input_schema: json!({"type": "object"}),
            },
        ],
    }
}

fn options(mock: &Mock) -> AnthropicOptions {
    AnthropicOptions {
        base_url: mock.url.clone(),
        settings: settings(),
        timing: Timing {
            idle_timeout: IDLE,
            ..Timing::default()
        },
    }
}

type TestProvider = AnthropicProvider<RecordingSleeper, DrivenClock>;

fn secret_env(name: &str) -> Option<String> {
    (name == KEY_ENV).then(|| SECRET.to_owned())
}

fn provider_with_env(
    mock: &Mock,
    env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
) -> (TestProvider, RecordingSleeper, DrivenClock) {
    let (sleeper, clock) = (RecordingSleeper::default(), DrivenClock::new());
    let built = AnthropicProvider::new(options(mock), KEY_ENV, env, sleeper.clone(), clock.clone())
        .expect("provider");
    (built, sleeper, clock)
}

fn provider(mock: &Mock) -> (TestProvider, RecordingSleeper, DrivenClock) {
    provider_with_env(mock, secret_env)
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

fn usage_and_stop(reason: StopReason) -> [ModelEvent; 2] {
    [
        ModelEvent::Usage(Usage {
            input_tokens: 3,
            output_tokens: 2,
        }),
        ModelEvent::Stop { reason },
    ]
}

fn error_text(collected: &Collected) -> String {
    collected
        .error
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default()
}

// ---- retry and classification ---------------------------------------------

#[tokio::test]
async fn a_529_before_any_output_retries_and_succeeds() {
    // Arrange
    let mock = start(vec![status(529), sse(&text_stream(&["hi"]))]);

    // Act
    let (collected, sleeper) = run(&mock).await;

    // Assert
    assert_eq!(collected.error, None);
    let mut expected = vec![text("hi")];
    expected.extend(usage_and_stop(StopReason::EndTurn));
    assert_eq!(collected.events, expected);
    assert_eq!(mock.attempts(), 2);
    assert_eq!(sleeper.retry_delays(), [Duration::from_millis(500)]);
}

#[tokio::test]
async fn three_529s_report_the_error_after_exactly_three_attempts() {
    // Arrange
    let mock = start(vec![status(529), status(529), status(529), status(529)]);

    // Act
    let (collected, sleeper) = run(&mock).await;

    // Assert
    assert!(matches!(
        collected.error,
        Some(ProviderError::Retryable { .. })
    ));
    assert_eq!(mock.attempts(), 3);
    assert_eq!(
        sleeper.retry_delays(),
        [Duration::from_millis(500), Duration::from_millis(1000)]
    );
}

#[tokio::test]
async fn server_errors_500_and_503_are_retried() {
    for code in [500, 503] {
        // Arrange
        let mock = start(vec![status(code), sse(&text_stream(&["ok"]))]);

        // Act
        let (collected, _sleeper) = run(&mock).await;

        // Assert
        assert_eq!(collected.error, None, "status {code}");
        assert_eq!(mock.attempts(), 2, "status {code}");
    }
}

#[tokio::test]
async fn client_errors_400_401_408_and_409_are_fatal_and_not_retried() {
    for code in [401, 400, 408, 409] {
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
async fn a_context_overflow_400_is_reported_as_context_window_exceeded() {
    // Arrange
    let mock = start(vec![Reply::Http {
        status: 400,
        headers: Vec::new(),
        body: json!({"error": {"type": "invalid_request_error",
            "message": "prompt is too long: 250000 tokens"}})
        .to_string(),
    }]);

    // Act
    let (collected, _sleeper) = run(&mock).await;

    // Assert
    assert_eq!(collected.error, Some(ProviderError::ContextWindowExceeded));
    assert_eq!(mock.attempts(), 1);
}

#[tokio::test]
async fn a_429_waits_for_the_retry_after_header() {
    // Arrange
    let mock = start(vec![
        Reply::Http {
            status: 429,
            headers: vec![("retry-after", "7".to_owned())],
            body: String::new(),
        },
        sse(&text_stream(&["ok"])),
    ]);

    // Act
    let (collected, sleeper) = run(&mock).await;

    // Assert
    assert_eq!(collected.error, None);
    assert_eq!(sleeper.retry_delays(), [Duration::from_secs(7)]);
}

#[tokio::test]
async fn an_empty_stream_is_retried_then_reported() {
    // Arrange
    let mock = start(vec![
        Reply::Sse(Vec::new()),
        Reply::Sse(Vec::new()),
        Reply::Sse(Vec::new()),
    ]);

    // Act
    let (collected, _sleeper) = run(&mock).await;

    // Assert
    assert!(matches!(
        collected.error,
        Some(ProviderError::Retryable { .. })
    ));
    assert_eq!(mock.attempts(), 3);
}

// ---- streams ---------------------------------------------------------------

#[tokio::test]
async fn a_drop_after_output_is_not_retried_and_reports_a_partial_turn() {
    // Arrange
    let partial = message_start() + &text_block("par");
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
async fn an_event_split_across_chunks_and_inside_a_multibyte_character_is_read() {
    // Arrange
    let bytes = text_stream(&["caf\u{e9} ", "\u{1f600}"]).into_bytes();
    let steps = bytes.iter().map(|byte| Step::Bytes(vec![*byte])).collect();
    let mock = start(vec![Reply::Sse(steps)]);

    // Act
    let (collected, _sleeper) = run(&mock).await;

    // Assert
    assert_eq!(collected.error, None);
    assert_eq!(
        collected.events[..2],
        [text("caf\u{e9} "), text("\u{1f600}")]
    );
}

#[tokio::test]
async fn tool_arguments_split_across_input_json_deltas_are_streamed() {
    // Arrange
    let mock = start(vec![sse(&tool_stream())]);

    // Act
    let (collected, _sleeper) = run(&mock).await;

    // Assert
    assert_eq!(collected.error, None);
    let fragment = |piece: &str| ModelEvent::ToolCallArgumentsDelta {
        call_id: "c1".to_owned(),
        fragment: piece.to_owned(),
    };
    let mut expected = vec![
        ModelEvent::ToolCallStart {
            call_id: "c1".to_owned(),
            name: "read".to_owned(),
        },
        fragment("{\"path\":"),
        fragment("\"a.txt\"}"),
    ];
    expected.extend(usage_and_stop(StopReason::ToolUse));
    assert_eq!(collected.events, expected);
}

#[tokio::test]
async fn an_error_event_inside_the_stream_ends_it_with_the_classified_error() {
    // Arrange
    let broken = message_start()
        + &text_block("a")
        + &frame(&json!({"type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}}));
    let mock = start(vec![sse(&broken)]);

    // Act
    let (collected, _sleeper) = run(&mock).await;

    // Assert
    assert_eq!(collected.events, [text("a")]);
    assert!(error_text(&collected).contains("overloaded_error"));
    assert_eq!(mock.attempts(), 1);
}

#[tokio::test]
async fn a_max_tokens_stop_is_reported() {
    // Arrange
    let stream = message_start() + &text_block("cut") + &end("max_tokens");
    let mock = start(vec![sse(&stream)]);

    // Act
    let (collected, _sleeper) = run(&mock).await;

    // Assert
    assert_eq!(
        collected.events.last(),
        Some(&ModelEvent::Stop {
            reason: StopReason::MaxTokens
        })
    );
}

// ---- idle timeout ---------------------------------------------------------

#[tokio::test]
async fn a_stalled_stream_is_aborted_once_the_driven_clock_passes_the_idle_timeout() {
    // Arrange
    let first = message_start() + &text_block("a");
    let mock = start(vec![Reply::Sse(vec![
        Step::Bytes(first.into_bytes()),
        Step::Hang,
    ])]);
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
async fn a_slow_but_alive_stream_is_not_aborted() {
    // Arrange
    let (gate_one, gate_two) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let bytes = |text: String| Step::Bytes(text.into_bytes());
    let mock = start(vec![Reply::Sse(vec![
        bytes(message_start() + &text_block("a")),
        Step::Wait(Arc::clone(&gate_one)),
        bytes(text_block("b")),
        Step::Wait(Arc::clone(&gate_two)),
        bytes(text_block("c") + &end("end_turn")),
    ])]);
    let (provider, _sleeper, clock) = provider(&mock);
    let mut stream = provider.stream(request()).await.expect("stream");
    let almost = Duration::from_secs(9);

    // Act
    let mut seen = Vec::new();
    for gate in [&gate_one, &gate_two] {
        let item = tokio::time::timeout(WAIT, stream.next())
            .await
            .expect("event");
        seen.push(item.expect("item").expect("ok"));
        clock.advance(almost);
        gate.notify_one();
    }
    while let Some(item) = tokio::time::timeout(WAIT, stream.next())
        .await
        .expect("event")
    {
        seen.push(item.expect("no abort"));
    }

    // Assert
    assert_eq!(seen[..3], [text("a"), text("b"), text("c")]);
    assert_eq!(seen.last(), Some(&usage_and_stop(StopReason::EndTurn)[1]));
}

// ---- request, key, contract ----------------------------------------------

#[tokio::test]
async fn max_output_tokens_reaches_the_body_the_registry_sends() {
    // Arrange: an empty section keeps the built-in limit.
    let cases = [("", 8192), ("max_output_tokens = 2048\n", 2048)];
    for (keys, limit) in cases {
        let mock = start(vec![sse(&text_stream(&["ok"]))]);
        let text = format!(
            "[providers.anthropic]\nenabled = true\napi_key_env = \"{KEY_ENV}\"\n\
             base_url = \"{}\"\n{keys}",
            mock.url
        );
        let registry = ModelRegistry::from_config(&Config::from_toml_str(&text).unwrap().config);
        let target = registry.select(BackendKind::Anthropic, None).unwrap();
        let client = registry.build(&target, secret_env).unwrap();

        // Act
        let collected = collect(&client, ModelRequest::user_prompt("hello"), WAIT).await;

        // Assert
        assert_eq!(collected.error, None);
        let requests = mock.requests.lock().expect("lock");
        assert_eq!(requests[0].body["max_tokens"], limit, "{keys}");
    }
}

#[tokio::test]
async fn the_request_carries_cache_breakpoints_and_the_key_header() {
    // Arrange
    let mock = start(vec![sse(&text_stream(&["ok"]))]);

    // Act
    let (collected, _sleeper) = run(&mock).await;

    // Assert
    assert_eq!(collected.error, None);
    let requests = mock.requests.lock().expect("lock");
    let sent = &requests[0];
    assert!(sent.head.starts_with("POST /v1/messages "));
    assert_eq!(sent.header("x-api-key"), Some(SECRET));
    assert!(sent.header("anthropic-version").is_some());
    assert_eq!(sent.body["stream"], true);
    assert_eq!(sent.body["messages"][0]["content"], "hello");
    assert_eq!(sent.body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(sent.body["tools"][0]["name"], "read");
    assert!(sent.body["tools"][0].get("cache_control").is_none());
    assert_eq!(sent.body["tools"][1]["cache_control"]["type"], "ephemeral");
}

#[tokio::test]
async fn a_rotated_key_is_picked_up_on_the_next_call() {
    // Arrange
    let mock = start(vec![sse(&text_stream(&["a"])), sse(&text_stream(&["b"]))]);
    let current = Arc::new(Mutex::new("key-one".to_owned()));
    let reader = Arc::clone(&current);
    let (provider, _sleeper, _clock) =
        provider_with_env(&mock, move |_| Some(reader.lock().expect("lock").clone()));

    // Act
    collect(&provider, request(), WAIT).await;
    *current.lock().expect("lock") = "key-two".to_owned();
    collect(&provider, request(), WAIT).await;

    // Assert
    let requests = mock.requests.lock().expect("lock");
    assert_eq!(requests[0].header("x-api-key"), Some("key-one"));
    assert_eq!(requests[1].header("x-api-key"), Some("key-two"));
}

#[tokio::test]
async fn the_key_never_appears_in_debug_or_error_output() {
    // Arrange
    let mock = start(Vec::new());
    let (good, _sleeper, _clock) = provider(&mock);
    let (bad, _sleeper, _clock) = provider_with_env(&mock, |_| Some(format!("{SECRET}\nsplit")));

    // Act
    let collected = collect(&bad, request(), WAIT).await;

    // Assert
    assert!(!format!("{good:?}").contains(SECRET));
    assert!(matches!(collected.error, Some(ProviderError::Fatal { .. })));
    assert!(!error_text(&collected).contains(SECRET));
    assert_eq!(mock.attempts(), 0);
}

fn named_key_config() -> ProviderConfig {
    ProviderConfig {
        api_key_env: Some(KEY_ENV.to_owned()),
        ..ProviderConfig::default()
    }
}

fn plain_options() -> AnthropicOptions {
    AnthropicOptions {
        base_url: "http://127.0.0.1:1".to_owned(),
        settings: settings(),
        timing: Timing::default(),
    }
}

#[test]
fn from_config_reads_the_key_from_the_named_variable() {
    // Arrange

    // Act
    let built = AnthropicProvider::from_config(&named_key_config(), plain_options(), secret_env);

    // Assert
    let provider = built.expect("provider");
    assert!(!format!("{provider:?}").contains(SECRET));
}

#[test]
fn from_config_rejects_a_missing_variable_or_name_without_leaking() {
    // Arrange
    let unnamed = ProviderConfig::default();
    let cases = [(named_key_config(), KEY_ENV), (unnamed, "api_key_env")];

    for (config, expected) in cases {
        // Act
        let result = AnthropicProvider::from_config(&config, plain_options(), |_| None);

        // Assert
        let error = result.expect_err("no key");
        assert!(matches!(error, ProviderError::Fatal { .. }));
        assert!(error.to_string().contains(expected), "{error}");
    }
}

fn scenario_mock(scenario: Scenario) -> Mock {
    let overflow = json!({"error": {"type": "invalid_request_error",
        "message": "prompt is too long"}})
    .to_string();
    start(match scenario {
        Scenario::Text | Scenario::DropCancels => vec![sse(&text_stream(&["Hel", "lo"]))],
        Scenario::ToolCall => vec![sse(&tool_stream())],
        Scenario::ContextWindowExceeded => vec![Reply::Http {
            status: 400,
            headers: Vec::new(),
            body: overflow,
        }],
        Scenario::Retryable => vec![status(529), status(529), status(529)],
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

#[tokio::test]
async fn a_redirect_is_fatal_and_never_followed() {
    // Arrange
    let target = start(vec![sse(&text_stream(&["leaked"]))]);
    let location = format!("{}/v1/messages", target.url);
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
    assert_eq!(mock.attempts(), 1);
    let sent = mock.flooded.load(Ordering::SeqCst);
    assert!(sent >= ERROR_BODY_CAP, "{sent}");
    assert!(sent < FLOOD_BYTES / 4, "the server could send {sent} bytes");
}

#[test]
fn a_base_url_must_be_https_or_loopback_http_without_credentials() {
    let cases = [
        ("https://api.anthropic.com", Ok(())),
        ("https://api.anthropic.com/", Ok(())),
        ("http://127.0.0.1:8080", Ok(())),
        ("http://127.9.9.9", Ok(())),
        ("http://[::1]:9", Ok(())),
        ("http://localhost:9", Ok(())),
        (
            "http://api.anthropic.com",
            Err(BaseUrlError::InsecureRemote),
        ),
        ("http://10.0.0.1", Err(BaseUrlError::InsecureRemote)),
        (
            "http://localhost.evil.test",
            Err(BaseUrlError::InsecureRemote),
        ),
        (
            "https://user:pw@api.anthropic.com",
            Err(BaseUrlError::Credentials),
        ),
        (
            "https://user@api.anthropic.com",
            Err(BaseUrlError::Credentials),
        ),
        (
            "ftp://api.anthropic.com",
            Err(BaseUrlError::UnsupportedScheme),
        ),
        ("file:///etc/passwd", Err(BaseUrlError::UnsupportedScheme)),
        ("not a url", Err(BaseUrlError::Invalid)),
    ];

    for (url, expected) in cases {
        // Act
        let result = validate_base_url(url).map(|_| ());

        // Assert
        assert_eq!(result, expected, "{url}");
    }
}

#[test]
fn the_constructor_rejects_an_unsafe_base_url_without_echoing_it() {
    // Arrange
    let mut unsafe_options = plain_options();
    unsafe_options.base_url = "https://user:hunter2@api.anthropic.com".to_owned();

    // Act
    let result = AnthropicProvider::from_config(&named_key_config(), unsafe_options, secret_env);

    // Assert
    let error = result.expect_err("credentials in the URL");
    assert!(matches!(error, ProviderError::Fatal { .. }));
    assert!(!error.to_string().contains("hunter2"));
}

#[test]
fn the_constructor_names_the_provider_in_an_insecure_base_url_refusal() {
    // Arrange
    let mut insecure = plain_options();
    insecure.base_url = "http://example.com".to_owned();

    // Act
    let result = AnthropicProvider::new(
        insecure,
        KEY_ENV,
        secret_env,
        RecordingSleeper::default(),
        DrivenClock::new(),
    );

    // Assert
    let Err(error) = result else {
        panic!("plain http to a remote host was accepted");
    };
    assert_eq!(
        error,
        ProviderError::Fatal {
            message: "the Anthropic base URL must use https unless it points at a loopback host"
                .to_owned()
        }
    );
}

#[tokio::test]
async fn dropping_the_stream_closes_the_connection() {
    // Arrange
    let first = message_start() + &text_block("a");
    let mock = start(vec![Reply::Sse(vec![
        Step::Bytes(first.into_bytes()),
        Step::Hang,
    ])]);
    let (provider, _sleeper, _clock) = provider(&mock);
    let mut stream = provider.stream(request()).await.expect("stream");
    tokio::time::timeout(WAIT, stream.next())
        .await
        .expect("event");

    // Act
    drop(stream);

    // Assert
    tokio::time::timeout(WAIT, mock.closed.notified())
        .await
        .expect("the server saw the client disconnect");
}

#[tokio::test]
#[ignore = "needs a stream recorded from the real API under CODA_LIVE_TESTS; no recording exists yet"]
async fn a_recorded_real_stream_replays_to_the_expected_events() {
    unimplemented!("parked until a real stream is recorded");
}
