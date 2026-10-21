// SPDX-License-Identifier: AGPL-3.0-only

//! The openai-compat provider: streams a chat completions call, retrying only
//! while no output has been delivered.
//!
//! A retry after an idle timeout before the first byte may be billed twice,
//! because the server can have accepted and started the first request.

use std::future::Future;
use std::sync::Arc;
use std::time::SystemTime;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use tokio::sync::mpsc;
use tracing::{Instrument, debug, info_span, warn};

use super::classify_status;
use super::events::EventMapper;
use super::request::build_body;
use crate::anthropic::request::RequestSettings;
use crate::http::sse::SseParser;
use crate::http::{
    Idle, IdleWatch, KeySource, Timing, TransportWording, build_client, fatal, retry_after,
    validate_base_url,
};
use crate::model::{
    Clock, ModelEvent, ModelProvider, ModelRequest, ModelStream, ProviderError, Sleeper, StreamItem,
};

const STREAM_BUFFER: usize = 32;
const COMPLETIONS_PATH: &str = "/chat/completions";

/// Why a base URL is refused; it is never echoed, as it may hold credentials.
const BASE_URL_REFUSED: &str = "the openai-compat base URL must be a valid https URL, or http towards \
loopback, without credentials, query or fragment";
const BEARER_PREFIX: &str = "Bearer ";
const REDACTED: &str = "[redacted]";

const WORDING: TransportWording = TransportWording {
    idle: "the server sent nothing for longer than the idle timeout",
    build_failed: "the request could not be built",
    connect_failed: "could not connect to the server",
    timed_out: "the connection to the server timed out",
    failed: "the connection to the server failed",
};

/// The `Authorization` value, or `None` when no key is named or set: local
/// servers need none.
fn key_header(key: Option<&KeySource>) -> Result<Option<HeaderValue>, ProviderError> {
    let Some(key) = key else {
        return Ok(None);
    };
    let Some(value) = key.read() else {
        return Ok(None);
    };
    key.sensitive_header(&format!("{BEARER_PREFIX}{value}"))
        .map(Some)
}

/// Where and how the provider calls the API.
#[derive(Debug, Clone)]
pub struct OpenAiCompatOptions {
    /// The API root including any version prefix, e.g. `http://host:11434/v1`.
    pub base_url: String,
    /// The parts of the request that stay the same across calls.
    pub settings: RequestSettings,
    /// Retry and idle bounds.
    pub timing: Timing,
}

/// Streams model responses from an OpenAI-compatible chat completions API.
#[derive(Debug)]
pub struct OpenAiCompatProvider<S, C> {
    inner: Arc<Inner<S, C>>,
}

#[derive(Debug)]
struct Inner<S, C> {
    client: reqwest::Client,
    url: String,
    key: Option<KeySource>,
    settings: RequestSettings,
    timing: Timing,
    watch: IdleWatch<S, C>,
}

impl<S: Sleeper + 'static, C: Clock + 'static> OpenAiCompatProvider<S, C> {
    /// Builds a provider with explicit time seams.
    ///
    /// `key_env` names the variable `env` is asked for the key on each call;
    /// without a name, or when the variable is unset, no key is sent.
    ///
    /// # Errors
    /// Fails when the base URL is refused or the client cannot be built.
    pub fn new(
        options: OpenAiCompatOptions,
        key_env: Option<String>,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
        sleeper: S,
        clock: C,
    ) -> Result<Self, ProviderError> {
        let base = validate_base_url(&options.base_url)
            .ok()
            .filter(|base| base.query().is_none() && base.fragment().is_none())
            .ok_or_else(|| fatal(BASE_URL_REFUSED))?;
        let client = build_client()?;
        let url = format!("{}{COMPLETIONS_PATH}", base.as_str().trim_end_matches('/'));
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                url,
                key: key_env.map(|name| KeySource::new(name, env)),
                settings: options.settings,
                timing: options.timing,
                watch: IdleWatch::new(sleeper, clock, options.timing.idle_timeout),
            }),
        })
    }
}

impl<S: Sleeper + 'static, C: Clock + 'static> ModelProvider for OpenAiCompatProvider<S, C> {
    fn stream(
        &self,
        request: ModelRequest,
    ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send {
        let inner = Arc::clone(&self.inner);
        std::future::ready(Ok(ModelStream::spawn(
            STREAM_BUFFER,
            move |sender| async move {
                let span = info_span!("openai_compat.call", model = %inner.settings.model);
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
            let delay =
                self.timing
                    .retry_delay(attempt, failure.retry_after.as_deref(), SystemTime::now());
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
        let key_header = key_header(self.key.as_ref()).map_err(AttemptFailure::before_output)?;
        let mut pending = self
            .client
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_vec());
        if let Some(value) = &key_header {
            pending = pending.header(AUTHORIZATION, value.clone());
        }
        let response = self
            .watch
            .within_idle(pending.send())
            .await
            .map_err(|Idle| AttemptFailure::before_output(WORDING.idle_error()))?
            .map_err(|error| AttemptFailure::before_output(WORDING.transport_error(&error)))?;
        let status = response.status();
        if status.is_redirection() {
            return Err(AttemptFailure::before_output(fatal(format!(
                "the server answered with a redirect (HTTP {}), which is never followed",
                status.as_u16()
            ))));
        }
        if !status.is_success() {
            return Err(self.rejected(response, key_header.as_ref()).await);
        }
        self.read_stream(response, sender).await
    }

    async fn rejected(
        &self,
        mut response: reqwest::Response,
        key_header: Option<&HeaderValue>,
    ) -> AttemptFailure {
        let status = response.status().as_u16();
        let retry_after = retry_after(&response);
        let mut text = self.watch.read_error_body(&mut response).await;
        // A server may echo the credential it rejected into its message.
        if let Some(key) = key_header
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix(BEARER_PREFIX))
        {
            text = text.replace(key, REDACTED);
        }
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
                    fatal(format!("the server stream is malformed: {error}")),
                )
            })?;
            for payload in payloads {
                let step = mapper
                    .handle(&payload)
                    .map_err(|error| failed(&mapper, error))?;
                if !deliver(sender, step.events).await || step.done {
                    return Ok(());
                }
            }
            if at_end {
                let step = mapper
                    .end_of_stream()
                    .map_err(|error| failed(&mapper, error))?;
                deliver(sender, step.events).await;
                return Ok(());
            }
        }
    }
}

/// Sends the events in order; false once the consumer has gone away.
async fn deliver(sender: &mpsc::Sender<StreamItem>, events: Vec<ModelEvent>) -> bool {
    for event in events {
        if sender.send(Ok(event)).await.is_err() {
            return false;
        }
    }
    true
}
