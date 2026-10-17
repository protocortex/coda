// SPDX-License-Identifier: AGPL-3.0-only

//! The Anthropic Messages API: wire format, retry timing and status
//! classification.

use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::model::ProviderError;

pub mod events;
pub mod provider;
pub mod request;
pub mod sse;

/// Calls made before a retryable failure is reported.
const MAX_ATTEMPTS: u32 = 3;
/// Longest piece of an API error message kept in our own errors.
const MAX_MESSAGE_CHARS: usize = 200;
/// Error type names the API uses for failures that a later call may survive.
const RETRYABLE_ERROR_TYPES: [&str; 3] = ["overloaded_error", "api_error", "rate_limit_error"];
/// Message fragments by which the API reports a context overflow.
const OVERFLOW_MARKERS: [&str; 3] = [
    "prompt is too long",
    "context window",
    // Covers "input length and max_tokens exceed context limit".
    "exceed context limit",
];

/// Retry and idle bounds for provider calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Calls made before a retryable failure is reported.
    pub max_attempts: u32,
    /// Delay after the first failed call; doubles after each further one.
    pub backoff_base: Duration,
    /// Upper bound for any retry delay.
    pub backoff_max: Duration,
    /// How long a stream may stay silent before it is aborted.
    pub idle_timeout: Duration,
}

impl Timing {
    /// Takes the bounds from the daemon's timing configuration.
    #[must_use]
    pub const fn from_config(timing: &coda_config::timing::Timing) -> Self {
        Self {
            max_attempts: MAX_ATTEMPTS,
            backoff_base: timing.retry_backoff_base,
            backoff_max: timing.retry_backoff_max,
            idle_timeout: timing.idle_timeout,
        }
    }

    /// The delay before the call that follows `failed_attempts` failures.
    /// A `Retry-After` value (seconds or HTTP date, measured from `now`)
    /// wins over the backoff; either way the delay stops at `backoff_max`.
    #[must_use]
    pub fn retry_delay(
        &self,
        failed_attempts: u32,
        retry_after: Option<&str>,
        now: SystemTime,
    ) -> Duration {
        let requested = retry_after.and_then(|value| parse_retry_after(value, now));
        let backoff = self
            .backoff_base
            .saturating_mul(2u32.saturating_pow(failed_attempts.saturating_sub(1)));
        requested.unwrap_or(backoff).min(self.backoff_max)
    }
}

impl Default for Timing {
    fn default() -> Self {
        Self::from_config(&coda_config::timing::Timing::default())
    }
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(date.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Classifies a non-success HTTP status and its body.
#[must_use]
pub fn classify_status(status: u16, body: &str) -> ProviderError {
    let api_message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.pointer("/error/message")?.as_str().map(str::to_owned));
    if status == 413 {
        return ProviderError::ContextWindowExceeded;
    }
    if status == 400 && api_message.as_deref().is_some_and(is_context_overflow) {
        return ProviderError::ContextWindowExceeded;
    }
    let message = match api_message {
        Some(detail) => format!("Anthropic returned HTTP {status}: {}", shorten(&detail)),
        None => format!("Anthropic returned HTTP {status}"),
    };
    if matches!(status, 429 | 500..) {
        ProviderError::Retryable { message }
    } else {
        ProviderError::Fatal { message }
    }
}

/// Classifies an error the API reported inside a stream.
pub(crate) fn classify_stream_error(kind: &str, detail: &str) -> ProviderError {
    if is_context_overflow(detail) {
        return ProviderError::ContextWindowExceeded;
    }
    let message = format!("Anthropic stream error ({kind}): {}", shorten(detail));
    if RETRYABLE_ERROR_TYPES.contains(&kind) {
        ProviderError::Retryable { message }
    } else {
        ProviderError::Fatal { message }
    }
}

fn is_context_overflow(message: &str) -> bool {
    let lowered = message.to_lowercase();
    OVERFLOW_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

fn shorten(text: &str) -> String {
    text.chars().take(MAX_MESSAGE_CHARS).collect()
}
