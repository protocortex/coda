// SPDX-License-Identifier: AGPL-3.0-only

//! Retry and idle bounds for provider calls.

use std::time::{Duration, SystemTime};

/// Calls made before a retryable failure is reported.
const MAX_ATTEMPTS: u32 = 3;

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
