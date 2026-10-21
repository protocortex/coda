// SPDX-License-Identifier: AGPL-3.0-only

//! The OpenAI-compatible chat completions API: wire format and classification.

use serde_json::Value;

use crate::http::is_retryable_status;
use crate::model::ProviderError;

pub mod events;
pub mod provider;
pub mod request;

const MAX_MESSAGE_CHARS: usize = 200;
/// Message fragments by which servers report a context overflow.
const OVERFLOW_MARKERS: [&str; 4] = [
    "context length",
    "context window",
    "context size",
    "maximum context",
];
const OVERFLOW_CODE: &str = "context_length_exceeded";

/// Classifies a non-success HTTP status and its body.
#[must_use]
pub fn classify_status(status: u16, body: &str) -> ProviderError {
    // Some servers put the message at the top level instead of under `error`.
    let error = serde_json::from_str::<Value>(body)
        .ok()
        .map(|value| value.get("error").cloned().unwrap_or(value));
    if status == 413 || (status == 400 && error.as_ref().is_some_and(is_context_overflow)) {
        return ProviderError::ContextWindowExceeded;
    }
    let detail = error.as_ref().and_then(|error| error["message"].as_str());
    let message = match detail {
        Some(detail) => format!(
            "the server returned HTTP {status}: {}",
            detail.chars().take(MAX_MESSAGE_CHARS).collect::<String>()
        ),
        None => format!("the server returned HTTP {status}"),
    };
    if is_retryable_status(status) {
        ProviderError::Retryable { message }
    } else {
        ProviderError::Fatal { message }
    }
}

/// Whether an `error` object reports that the prompt did not fit.
pub(crate) fn is_context_overflow(error: &Value) -> bool {
    let message = error["message"].as_str().unwrap_or_default().to_lowercase();
    error["code"].as_str() == Some(OVERFLOW_CODE)
        || OVERFLOW_MARKERS
            .iter()
            .any(|marker| message.contains(marker))
}
