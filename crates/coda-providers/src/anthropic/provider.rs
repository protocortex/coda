// SPDX-License-Identifier: AGPL-3.0-only

//! The Anthropic provider: streams a Messages call, retrying only while no
//! output has been delivered.
//!
//! A retry after an idle timeout before the first byte may be billed twice,
//! because the server can have accepted and started the first request.

use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use coda_config::config::ProviderConfig;
use reqwest::header::{CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::{Url, redirect};
use tokio::sync::mpsc;
use tracing::{Instrument, debug, info_span, warn};

use super::events::EventMapper;
use super::request::{RequestSettings, build_body};
use super::sse::SseParser;
use super::{Timing, classify_status};
use crate::model::{
    Clock, ModelEvent, ModelProvider, ModelRequest, ModelStream, ProviderError, Sleeper,
    StreamItem, SystemClock, TokioSleeper,
};

/// How often a waiting call checks whether the stream has gone idle.
pub const IDLE_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// Events buffered between the network task and the consumer.
const STREAM_BUFFER: usize = 32;
/// Longest wait for a connection to be established.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Path of the streaming endpoint.
const MESSAGES_PATH: &str = "/v1/messages";
/// The API version this client speaks.
const API_VERSION: &str = "2023-06-01";
/// Most of an error response body that is read; the rest is dropped.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const KEY_HEADER: &str = "x-api-key";
const VERSION_HEADER: &str = "anthropic-version";

/// Why a base URL was refused. No variant carries the URL, which may hold
/// credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BaseUrlError {
    /// The text is not a URL.
    #[error("the Anthropic base URL is not a valid URL")]
    Invalid,
    /// The scheme is neither http nor https.
    #[error("the Anthropic base URL must use http or https")]
    UnsupportedScheme,
    /// The URL embeds a user name or password.
    #[error("the Anthropic base URL must not carry credentials")]
    Credentials,
    /// Plain http is only allowed towards the local machine.
    #[error("the Anthropic base URL must use https unless it points at a loopback host")]
    InsecureRemote,
}

/// Checks a base URL: https, or http towards loopback only, with no
/// credentials in it.
///
/// # Errors
/// Names the first rule the URL breaks.
pub fn validate_base_url(base_url: &str) -> Result<Url, BaseUrlError> {
    let url = Url::parse(base_url).map_err(|_| BaseUrlError::Invalid)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(BaseUrlError::UnsupportedScheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(BaseUrlError::Credentials);
    }
    if url.scheme() == "http" && !is_loopback(&url) {
        return Err(BaseUrlError::InsecureRemote);
    }
    Ok(url)
}

fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

/// Looks a variable up in the environment.
type EnvReader = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Reads the API key from the environment on every attempt, so a rotated
/// key applies to the next call. Only the variable name is kept.
struct KeySource {
    name: String,
    read: EnvReader,
}

impl fmt::Debug for KeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeySource")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl KeySource {
    fn header(&self) -> Result<HeaderValue, ProviderError> {
        let name = &self.name;
        let key = (self.read)(name).ok_or_else(|| {
            fatal(format!(
                "environment variable {name} holding the API key is not set"
            ))
        })?;
        let mut value = HeaderValue::from_str(&key).map_err(|_| {
            fatal(format!(
                "the API key in {name} is not valid in an HTTP header"
            ))
        })?;
        value.set_sensitive(true);
        Ok(value)
    }
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
    sleeper: S,
    clock: C,
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
        provider.inner.key.header()?;
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
        let base =
            validate_base_url(&options.base_url).map_err(|error| fatal(error.to_string()))?;
        // A redirect would carry the key header to whatever host answers.
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(redirect::Policy::none())
            .build()
            .map_err(|_| fatal("the HTTP client could not be built"))?;
        let url = format!("{}{MESSAGES_PATH}", base.as_str().trim_end_matches('/'));
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                url,
                key: KeySource {
                    name: key_env.into(),
                    read: Box::new(env),
                },
                settings: options.settings,
                timing: options.timing,
                sleeper,
                clock,
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

fn fatal(message: impl Into<String>) -> ProviderError {
    ProviderError::Fatal {
        message: message.into(),
    }
}

fn retryable(message: impl Into<String>) -> ProviderError {
    ProviderError::Retryable {
        message: message.into(),
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

/// The stream stayed silent longer than the idle timeout.
struct Idle;

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
            self.sleeper.sleep(delay).await;
            attempt += 1;
        }
    }

    async fn attempt(
        &self,
        body: &[u8],
        sender: &mpsc::Sender<StreamItem>,
    ) -> Result<(), AttemptFailure> {
        let key_header = self.key.header().map_err(AttemptFailure::before_output)?;
        let pending = self
            .client
            .post(&self.url)
            .header(KEY_HEADER, key_header)
            .header(VERSION_HEADER, API_VERSION)
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_vec())
            .send();
        let response = self
            .within_idle(pending, self.clock.now())
            .await
            .map_err(|Idle| AttemptFailure::before_output(idle_error()))?
            .map_err(|error| AttemptFailure::before_output(transport_error(&error)))?;
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
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let text = self.read_error_body(&mut response).await;
        AttemptFailure {
            error: classify_status(status, &text),
            output_started: false,
            retry_after,
        }
    }

    /// Reads at most `MAX_ERROR_BODY_BYTES` of an error body; what cannot be
    /// read in time or in full is simply left out.
    async fn read_error_body(&self, response: &mut reqwest::Response) -> String {
        let mut body = Vec::new();
        while body.len() < MAX_ERROR_BODY_BYTES {
            let Ok(Ok(Some(chunk))) = self.within_idle(response.chunk(), self.clock.now()).await
            else {
                break;
            };
            let room = MAX_ERROR_BODY_BYTES - body.len();
            body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        }
        String::from_utf8_lossy(&body).into_owned()
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
                .within_idle(response.chunk(), self.clock.now())
                .await
                .map_err(|Idle| failed(&mapper, idle_error()))?
                .map_err(|error| failed(&mapper, transport_error(&error)))?;
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

    /// Awaits `future`, giving up once the clock shows `idle_timeout` has
    /// passed since `since`. Time is checked on a tick so a test drives it
    /// through the injected clock.
    async fn within_idle<F: Future>(&self, future: F, since: Instant) -> Result<F::Output, Idle> {
        tokio::pin!(future);
        loop {
            tokio::select! {
                output = &mut future => return Ok(output),
                () = self.sleeper.sleep(IDLE_CHECK_INTERVAL) => {
                    let silent = self.clock.now().saturating_duration_since(since);
                    if silent >= self.timing.idle_timeout {
                        return Err(Idle);
                    }
                }
            }
        }
    }
}

async fn deliver(sender: &mpsc::Sender<StreamItem>, event: ModelEvent) -> bool {
    sender.send(Ok(event)).await.is_ok()
}

fn idle_error() -> ProviderError {
    retryable("Anthropic sent nothing for longer than the idle timeout")
}

/// Describes a transport failure without the URL or any header.
fn transport_error(error: &reqwest::Error) -> ProviderError {
    if error.is_builder() {
        return fatal("the Anthropic request could not be built");
    }
    let what = if error.is_connect() {
        "could not connect to Anthropic"
    } else if error.is_timeout() {
        "the Anthropic connection timed out"
    } else {
        "the Anthropic connection failed"
    };
    retryable(what)
}
