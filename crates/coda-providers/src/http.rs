// SPDX-License-Identifier: AGPL-3.0-only

//! The HTTP transport the streaming providers share: the client, base URL
//! rules, key handling, idle detection and transport error mapping.

use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::time::Duration;

use reqwest::header::{HeaderValue, RETRY_AFTER};
use reqwest::{Response, Url, redirect};

use crate::model::{Clock, ProviderError, Sleeper};

pub mod sse;
mod timing;

pub use timing::Timing;

/// How often a waiting call checks whether the stream has gone idle.
pub const IDLE_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// Longest wait for a connection to be established.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Most of an error response body that is read; the rest is dropped.
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Why a base URL was refused. No variant carries the URL, which may hold
/// credentials. Each text continues a sentence naming the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BaseUrlError {
    /// The text is not a URL.
    #[error("base URL is not a valid URL")]
    Invalid,
    /// The scheme is neither http nor https.
    #[error("base URL must use http or https")]
    UnsupportedScheme,
    /// The URL embeds a user name or password.
    #[error("base URL must not carry credentials")]
    Credentials,
    /// Plain http is only allowed towards the local machine.
    #[error("base URL must use https unless it points at a loopback host")]
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

pub(crate) fn fatal(message: impl Into<String>) -> ProviderError {
    ProviderError::Fatal {
        message: message.into(),
    }
}

pub(crate) fn retryable(message: impl Into<String>) -> ProviderError {
    ProviderError::Retryable {
        message: message.into(),
    }
}

/// Whether a non-success status is worth another try.
pub(crate) const fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500..)
}

/// Builds the client every provider call goes through.
///
/// Redirects are off because one would carry the key header to whatever host
/// answers.
pub(crate) fn build_client() -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(redirect::Policy::none())
        .build()
        .map_err(|_| fatal("the HTTP client could not be built"))
}

/// The `Retry-After` header of a response, if it is readable text.
pub(crate) fn retry_after(response: &Response) -> Option<String> {
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Looks a variable up in the environment.
type EnvReader = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Reads an API key from the environment on every attempt, so a rotated key
/// applies to the next call. Only the variable name is kept.
pub(crate) struct KeySource {
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
    pub(crate) fn new(
        name: impl Into<String>,
        read: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            read: Box::new(read),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The key as it is now, or `None` when the variable is unset.
    pub(crate) fn read(&self) -> Option<String> {
        (self.read)(&self.name)
    }

    /// Wraps `value` as a header value that is kept out of debug output.
    pub(crate) fn sensitive_header(&self, value: &str) -> Result<HeaderValue, ProviderError> {
        let mut header = HeaderValue::from_str(value).map_err(|_| {
            fatal(format!(
                "the API key in {} is not valid in an HTTP header",
                self.name
            ))
        })?;
        header.set_sensitive(true);
        Ok(header)
    }
}

/// How a provider words the failures of its connection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TransportWording {
    pub idle: &'static str,
    pub build_failed: &'static str,
    pub connect_failed: &'static str,
    pub timed_out: &'static str,
    pub failed: &'static str,
}

impl TransportWording {
    /// The stream stayed silent past the idle timeout.
    pub(crate) fn idle_error(&self) -> ProviderError {
        retryable(self.idle)
    }

    /// Describes a transport failure without the URL or any header. A request
    /// that could not be built is fatal; every other failure may pass.
    pub(crate) fn transport_error(&self, error: &reqwest::Error) -> ProviderError {
        if error.is_builder() {
            return fatal(self.build_failed);
        }
        let what = if error.is_connect() {
            self.connect_failed
        } else if error.is_timeout() {
            self.timed_out
        } else {
            self.failed
        };
        retryable(what)
    }
}

/// The stream stayed silent longer than the idle timeout.
pub(crate) struct Idle;

/// Watches the clock so a silent connection is given up on. The time seams
/// are injected so a test drives them.
#[derive(Debug)]
pub(crate) struct IdleWatch<S, C> {
    sleeper: S,
    clock: C,
    idle_timeout: Duration,
}

impl<S: Sleeper, C: Clock> IdleWatch<S, C> {
    pub(crate) const fn new(sleeper: S, clock: C, idle_timeout: Duration) -> Self {
        Self {
            sleeper,
            clock,
            idle_timeout,
        }
    }

    pub(crate) fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send {
        self.sleeper.sleep(duration)
    }

    /// Awaits `future`, giving up once `idle_timeout` has passed on the clock
    /// since the call began. Time is checked on a tick so a test drives it
    /// through the injected clock.
    pub(crate) async fn within_idle<F: Future>(&self, future: F) -> Result<F::Output, Idle> {
        let since = self.clock.now();
        tokio::pin!(future);
        loop {
            tokio::select! {
                output = &mut future => return Ok(output),
                () = self.sleeper.sleep(IDLE_CHECK_INTERVAL) => {
                    let silent = self.clock.now().saturating_duration_since(since);
                    if silent >= self.idle_timeout {
                        return Err(Idle);
                    }
                }
            }
        }
    }

    /// Reads at most `MAX_ERROR_BODY_BYTES` of an error body; what cannot be
    /// read in time or in full is simply left out.
    pub(crate) async fn read_error_body(&self, response: &mut Response) -> String {
        let mut body = Vec::new();
        while body.len() < MAX_ERROR_BODY_BYTES {
            let Ok(Ok(Some(chunk))) = self.within_idle(response.chunk()).await else {
                break;
            };
            let room = MAX_ERROR_BODY_BYTES - body.len();
            body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        }
        String::from_utf8_lossy(&body).into_owned()
    }
}

#[cfg(test)]
mod tests;
