// SPDX-License-Identifier: Apache-2.0

//! The Anthropic Messages API: wire format, retry timing and status
//! classification.

use serde_json::Value;

use crate::http::is_retryable_status;
use crate::model::ProviderError;

pub mod events;
pub mod provider;
pub mod request;

pub use crate::http::{Timing, sse};

/// Longest piece of an API error message kept in our own errors.
const MAX_MESSAGE_CHARS: usize = 200;
/// Error type names the API uses for failures that a later call may survive.
const HTTP_NOT_FOUND: u16 = 404;
const NOT_FOUND_ERROR_TYPE: &str = "not_found_error";
const RETRYABLE_ERROR_TYPES: [&str; 3] = ["overloaded_error", "api_error", "rate_limit_error"];
/// Message fragments by which the API reports a context overflow.
const OVERFLOW_MARKERS: [&str; 3] = [
    "prompt is too long",
    "context window",
    // Covers "input length and max_tokens exceed context limit".
    "exceed context limit",
];

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
    // A 404 with no such body is a wrong base URL, which no other model fixes.
    let names_missing_model = serde_json::from_str::<Value>(body).is_ok_and(|value| {
        value.pointer("/error/type").and_then(Value::as_str) == Some(NOT_FOUND_ERROR_TYPE)
    });
    if status == HTTP_NOT_FOUND && names_missing_model {
        ProviderError::UnknownModel { message }
    } else if is_retryable_status(status) {
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
    if kind == NOT_FOUND_ERROR_TYPE {
        ProviderError::UnknownModel { message }
    } else if RETRYABLE_ERROR_TYPES.contains(&kind) {
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
