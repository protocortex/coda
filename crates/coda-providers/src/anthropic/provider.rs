// SPDX-License-Identifier: AGPL-3.0-only

//! The Anthropic provider: streams a Messages call, retrying only while no
//! output has been delivered.
//!
//! A retry after an idle timeout before the first byte may be billed twice,
//! because the server can have accepted and started the first request.

use std::future::Future;
use std::sync::Arc;

use coda_config::config::ProviderConfig;
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use tokio::sync::mpsc;
use tracing::{Instrument, debug, info_span, warn};

use super::classify_status;
use super::events::EventMapper;
use super::request::{RequestSettings, build_body};
use crate::http::sse::SseParser;
use crate::http::{
    Idle, IdleWatch, KeySource, Timing, TransportWording, build_client, fatal, retry_after,
    retryable,
};
use crate::model::{
    Clock, ModelEvent, ModelProvider, ModelRequest, ModelStream, ProviderError, Sleeper,
    StreamItem, SystemClock, TokioSleeper,
};

/// The Display text of `BaseUrlError` no longer carries the provider name;
/// the provider adds it ("the Anthropic ...").
pub use crate::http::{BaseUrlError, IDLE_CHECK_INTERVAL, validate_base_url};

/// Events buffered between the network task and the consumer.
const STREAM_BUFFER: usize = 32;
/// Path of the streaming endpoint.
const MESSAGES_PATH: &str = "/v1/messages";
/// The API version this client speaks.
const API_VERSION: &str = "2023-06-01";
const KEY_HEADER: &str = "x-api-key";
const VERSION_HEADER: &str = "anthropic-version";

const WORDING: TransportWording = TransportWording {
    idle: "Anthropic sent nothing for longer than the idle timeout",
    build_failed: "the Anthropic request could not be built",
    connect_failed: "could not connect to Anthropic",
    timed_out: "the Anthropic connection timed out",
    failed: "the Anthropic connection failed",
};

/// Reads the API key and wraps it as the key header.
fn key_header(key: &KeySource) -> Result<HeaderValue, ProviderError> {
    let name = key.name();
    let value = key.read().ok_or_else(|| {
        fatal(format!(
            "environment variable {name} holding the API key is not set"
        ))
    })?;
    key.sensitive_header(&value)
}

/// Where and how the provider calls the API.
#[derive(Debug, Clone)]
pub struct AnthropicOptions {
    /// Scheme and host of the API, without a path.
    pub base_url: String,
    /// The parts of the request that stay the same across calls.
    pub settings: RequestSettings,
    /// Retry and idle bounds.
    pub timing: Timing,
}

/// Streams model responses from the Anthropic Messages API.
#[derive(Debug)]
pub struct AnthropicProvider<S, C> {
    inner: Arc<Inner<S, C>>,
}

#[derive(Debug)]
struct Inner<S, C> {
    client: reqwest::Client,
    url: String,
    key: KeySource,
    settings: RequestSettings,
    timing: Timing,
    watch: IdleWatch<S, C>,
}

impl AnthropicProvider<TokioSleeper, SystemClock> {
    /// Builds a provider with the real clock, reading the key from the
    /// environment variable the configuration names.
    ///
    /// # Errors
    /// Fails when no variable is named, it is unset, the base URL is refused,
    /// or the client cannot be built.
    pub fn from_config(
        config: &ProviderConfig,
        options: AnthropicOptions,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Result<Self, ProviderError> {
        let name = config.api_key_env.as_deref().ok_or_else(|| {
            fatal("the Anthropic provider needs api_key_env to name its key variable")
        })?;
        let provider = Self::new(options, name, env, TokioSleeper, SystemClock)?;
        key_header(&provider.inner.key)?;
        Ok(provider)
    }
}

impl<S: Sleeper + 'static, C: Clock + 'static> AnthropicProvider<S, C> {
    /// Builds a provider with explicit time seams.
    ///
    /// `key_env` names the variable `env` is asked for the key on each call.
    ///
    /// # Errors
    /// Fails when the base URL is refused or the client cannot be built.
    pub fn new(
        options: AnthropicOptions,
        key_env: impl Into<String>,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
        sleeper: S,
        clock: C,
    ) -> Result<Self, ProviderError> {
        let base = validate_base_url(&options.base_url)
            .map_err(|error| fatal(format!("the Anthropic {error}")))?;
        let client = build_client()?;
        let url = format!("{}{MESSAGES_PATH}", base.as_str().trim_end_matches('/'));
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                url,
                key: KeySource::new(key_env, env),
                settings: options.settings,
                timing: options.timing,
                watch: IdleWatch::new(sleeper, clock, options.timing.idle_timeout),
            }),
        })
    }
}

impl<S: Sleeper + 'static, C: Clock + 'static> ModelProvider for AnthropicProvider<S, C> {
    fn stream(
        &self,
        request: ModelRequest,
    ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send {
        let inner = Arc::clone(&self.inner);
        std::future::ready(Ok(ModelStream::spawn(
            STREAM_BUFFER,
            move |sender| async move {
                let span = info_span!("anthropic.call", model = %inner.settings.model);
                let result = inner.call(&request, &sender).instrument(span).await;
                if let Err(error) = result {
                    let _ = sender.send(Err(error)).await;
                }
            },
        )))
    }
}

/// One failed attempt, with what the retry decision needs.
struct AttemptFailure {
    error: ProviderError,
    output_started: bool,
    retry_after: Option<String>,
}

impl AttemptFailure {
    const fn before_output(error: ProviderError) -> Self {
        Self {
            error,
            output_started: false,
            retry_after: None,
        }
    }
}

impl<S: Sleeper, C: Clock> Inner<S, C> {
    /// Runs the call: attempts until one succeeds, fails for good, or runs
    /// out of tries. A turn that already delivered output is never retried.
    async fn call(
        &self,
        request: &ModelRequest,
        sender: &mpsc::Sender<StreamItem>,
    ) -> Result<(), ProviderError> {
        let body = serde_json::to_vec(&build_body(&self.settings, request))
            .map_err(|_| fatal("the request could not be encoded"))?;
        let mut attempt = 1;
        loop {
            debug!(attempt, "sending request");
            let Err(failure) = self.attempt(&body, sender).await else {
                return Ok(());
            };
            if failure.output_started {
                return Err(fatal(format!(
                    "partial turn: the response broke off after output began: {}",
                    failure.error
                )));
            }
            if !failure.error.is_retryable() || attempt >= self.timing.max_attempts {
                return Err(failure.error);
            }
            let delay = self.timing.retry_delay(
                attempt,
                failure.retry_after.as_deref(),
                std::time::SystemTime::now(),
            );
            warn!(attempt, ?delay, "retrying after a retryable failure");
            self.watch.sleep(delay).await;
            attempt += 1;
        }
    }

    async fn attempt(
        &self,
        body: &[u8],
        sender: &mpsc::Sender<StreamItem>,
    ) -> Result<(), AttemptFailure> {
        let key_header = key_header(&self.key).map_err(AttemptFailure::before_output)?;
        let pending = self
            .client
            .post(&self.url)
            .header(KEY_HEADER, key_header)
            .header(VERSION_HEADER, API_VERSION)
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_vec())
            .send();
        let response = self
            .watch
            .within_idle(pending)
            .await
            .map_err(|Idle| AttemptFailure::before_output(WORDING.idle_error()))?
            .map_err(|error| AttemptFailure::before_output(WORDING.transport_error(&error)))?;
        let status = response.status();
        if status.is_redirection() {
            return Err(AttemptFailure::before_output(fatal(format!(
                "Anthropic answered with a redirect (HTTP {}), which is never followed",
                status.as_u16()
            ))));
        }
        if !status.is_success() {
            return Err(self.rejected(response).await);
        }
        self.read_stream(response, sender).await
    }

    async fn rejected(&self, mut response: reqwest::Response) -> AttemptFailure {
        let status = response.status().as_u16();
        let retry_after = retry_after(&response);
        let text = self.watch.read_error_body(&mut response).await;
        AttemptFailure {
            error: classify_status(status, &text),
            output_started: false,
            retry_after,
        }
    }

    async fn read_stream(
        &self,
        mut response: reqwest::Response,
        sender: &mpsc::Sender<StreamItem>,
    ) -> Result<(), AttemptFailure> {
        let mut parser = SseParser::default();
        let mut mapper = EventMapper::default();
        let failed = |mapper: &EventMapper, error| AttemptFailure {
            error,
            output_started: mapper.output_started(),
            retry_after: None,
        };
        loop {
            let chunk = self
                .watch
                .within_idle(response.chunk())
                .await
                .map_err(|Idle| failed(&mapper, WORDING.idle_error()))?
                .map_err(|error| failed(&mapper, WORDING.transport_error(&error)))?;
            let at_end = chunk.is_none();
            let payloads = match chunk {
                Some(bytes) => parser.push(&bytes),
                None => parser.finish(),
            }
            .map_err(|error| {
                failed(
                    &mapper,
                    fatal(format!("Anthropic stream is malformed: {error}")),
                )
            })?;
            for payload in payloads {
                let step = mapper
                    .handle(&payload)
                    .map_err(|error| failed(&mapper, error))?;
                for event in step.events {
                    if !deliver(sender, event).await {
                        return Ok(());
                    }
                }
                if step.done {
                    return Ok(());
                }
            }
            if at_end {
                return Err(failed(
                    &mapper,
                    retryable("Anthropic closed the stream before the message was complete"),
                ));
            }
        }
    }
}

async fn deliver(sender: &mpsc::Sender<StreamItem>, event: ModelEvent) -> bool {
    sender.send(Ok(event)).await.is_ok()
}
