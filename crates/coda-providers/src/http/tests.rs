// SPDX-License-Identifier: AGPL-3.0-only

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::{
    BaseUrlError, Idle, IdleWatch, KeySource, MAX_ERROR_BODY_BYTES, TransportWording, build_client,
    is_retryable_status, validate_base_url,
};
use crate::model::{Clock, ProviderError, SystemClock, TokioSleeper};

const WORDING: TransportWording = TransportWording {
    idle: "idle",
    build_failed: "build failed",
    connect_failed: "connect failed",
    timed_out: "timed out",
    failed: "failed",
};

async fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("address"));
    (listener, url)
}

/// Answers the first connection with `head` then `body`, after reading the request.
async fn answer_once(listener: TcpListener, head: String, body: Vec<u8>) {
    let (mut stream, _) = listener.accept().await.expect("accept");
    let mut buffer = [0u8; 1024];
    let _ = stream.read(&mut buffer).await;
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&body).await;
    let _ = stream.shutdown().await;
}

fn watch() -> IdleWatch<TokioSleeper, SystemClock> {
    IdleWatch::new(TokioSleeper, SystemClock, Duration::from_secs(5))
}

#[test]
fn https_and_loopback_http_base_urls_are_accepted() {
    for url in [
        "https://api.example.com",
        "http://localhost:11434/v1",
        "http://127.0.0.1:8080",
        "http://[::1]:8080",
    ] {
        // Act
        let result = validate_base_url(url);

        // Assert
        assert!(result.is_ok(), "{url} should be accepted");
    }
}

#[test]
fn unsafe_base_urls_are_refused_with_the_broken_rule() {
    for (url, expected) in [
        ("not a url", BaseUrlError::Invalid),
        ("ftp://example.com", BaseUrlError::UnsupportedScheme),
        ("https://user:pw@example.com", BaseUrlError::Credentials),
        ("http://example.com", BaseUrlError::InsecureRemote),
        ("http://10.0.0.5:8080", BaseUrlError::InsecureRemote),
    ] {
        // Act
        let result = validate_base_url(url);

        // Assert
        assert_eq!(result.err(), Some(expected), "{url}");
    }
}

#[tokio::test]
async fn the_client_does_not_follow_a_redirect() {
    // Arrange
    let (target, target_url) = listener().await;
    let (origin, origin_url) = listener().await;
    let head = format!("HTTP/1.1 302 Found\r\nlocation: {target_url}\r\ncontent-length: 0\r\n\r\n");
    tokio::spawn(answer_once(origin, head, Vec::new()));
    let client = build_client().expect("client");

    // Act
    let response = client.get(&origin_url).send().await.expect("response");

    // Assert
    assert_eq!(response.status().as_u16(), 302);
    let followed = tokio::time::timeout(Duration::from_millis(100), target.accept()).await;
    assert!(followed.is_err(), "the redirect target was contacted");
}

#[test]
fn only_throttling_and_server_statuses_are_retryable() {
    for (status, expected) in [
        (400, false),
        (401, false),
        (404, false),
        (408, false),
        (429, true),
        (500, true),
        (503, true),
    ] {
        // Act
        let retryable = is_retryable_status(status);

        // Assert
        assert_eq!(retryable, expected, "HTTP {status}");
    }
}

#[tokio::test]
async fn a_refused_connection_is_retryable_and_names_the_connect_failure() {
    // Arrange
    let (closed, url) = listener().await;
    drop(closed);
    let error = build_client()
        .expect("client")
        .get(&url)
        .send()
        .await
        .expect_err("nothing listens");

    // Act
    let mapped = WORDING.transport_error(&error);

    // Assert
    assert_eq!(
        mapped,
        ProviderError::Retryable {
            message: "connect failed".into()
        }
    );
}

#[tokio::test]
async fn a_client_timeout_is_retryable_and_names_the_timeout() {
    // Arrange
    let (silent, url) = listener().await;
    let error = reqwest::Client::builder()
        .timeout(Duration::from_millis(50))
        .build()
        .expect("client")
        .get(&url)
        .send()
        .await
        .expect_err("the server never answers");
    drop(silent);

    // Act
    let mapped = WORDING.transport_error(&error);

    // Assert
    assert_eq!(
        mapped,
        ProviderError::Retryable {
            message: "timed out".into()
        }
    );
}

#[tokio::test]
async fn a_request_that_cannot_be_built_is_fatal() {
    // Arrange
    let error = reqwest::Client::new()
        .get("not a url")
        .send()
        .await
        .expect_err("the URL is invalid");

    // Act
    let mapped = WORDING.transport_error(&error);

    // Assert
    assert_eq!(
        mapped,
        ProviderError::Fatal {
            message: "build failed".into()
        }
    );
}

#[test]
fn an_idle_stream_is_retryable() {
    // Act
    let error = WORDING.idle_error();

    // Assert
    assert!(error.is_retryable());
}

#[tokio::test]
async fn an_error_body_is_cut_at_the_cap() {
    // Arrange
    let (server, url) = listener().await;
    let body = vec![b'x'; MAX_ERROR_BODY_BYTES * 2];
    let head = format!(
        "HTTP/1.1 500 X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    tokio::spawn(answer_once(server, head, body));
    let mut response = build_client()
        .expect("client")
        .get(&url)
        .send()
        .await
        .expect("response");

    // Act
    let text = watch().read_error_body(&mut response).await;

    // Assert
    assert_eq!(text.len(), MAX_ERROR_BODY_BYTES);
}

#[test]
fn a_key_header_is_sensitive_and_a_bad_key_is_named_by_variable_only() {
    // Arrange
    let source = KeySource::new("MY_KEY", |_| None);

    // Act
    let header = source.sensitive_header("secret").expect("valid");
    let refused = source.sensitive_header("bad\nkey").expect_err("invalid");

    // Assert
    assert!(header.is_sensitive());
    let message = refused.to_string();
    assert!(message.contains("MY_KEY"));
    assert!(!message.contains("bad"));
}

/// A clock that follows tokio's paused time.
struct PausedClock {
    origin: Instant,
    started: tokio::time::Instant,
}

impl PausedClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            started: tokio::time::Instant::now(),
        }
    }
}

impl Clock for PausedClock {
    fn now(&self) -> Instant {
        self.origin + self.started.elapsed()
    }
}

#[tokio::test(start_paused = true)]
async fn a_silent_future_is_given_up_on_once_the_idle_timeout_passes() {
    // Arrange
    let watch = IdleWatch::new(TokioSleeper, PausedClock::new(), Duration::from_millis(500));

    // Act
    let result = watch.within_idle(std::future::pending::<()>()).await;

    // Assert
    assert!(matches!(result, Err(Idle)));
}
