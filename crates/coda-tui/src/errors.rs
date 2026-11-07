// SPDX-License-Identifier: Apache-2.0

//! The words for an error from a provider, and for a usage limit.
//!
//! No terminal code and no clock of its own, so every rule is tested without waiting.

use std::time::Duration;

use coda_protocol::events::UsageWindowKind;
use serde_json::Value;

use crate::work::format_duration;

/// Longest short form of an error, in characters.
const MAX_SHORT_CHARS: usize = 200;
/// How deep a message that holds another error object is followed.
const MAX_NESTING: usize = 3;
/// Longest run of words kept before a provider's message, in characters.
const MAX_PREFIX_CHARS: usize = 80;
const SECONDS_PER_DAY: u64 = 86_400;
const SECONDS_PER_HOUR: u64 = 3_600;
const LIMIT_REACHED: &str = "usage limit reached";
const WINDOW_FIVE_HOUR: &str = "five-hour window";
const WINDOW_SEVEN_DAY: &str = "seven-day window";
const WINDOW_MODEL_SCOPED: &str = "per-model window";
const WINDOW_MONTHLY: &str = "monthly window";
const RESET_PASSED: &str = "the window has reset";
const RESET_UNKNOWN: &str = "reset time unknown";

/// An error as the user first sees it, and all of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorText {
    /// One short line: the provider's own message when the text holds one.
    pub short: String,
    /// The whole text, for a key that expands it.
    pub full: String,
}

impl ErrorText {
    /// Whether there is more to show than the short form.
    #[must_use]
    pub fn expandable(&self) -> bool {
        self.short != self.full
    }
}

/// Reads an error text. Providers wrap their message in JSON, such as
/// `API Error: 400 {"type":"error","error":{"message":"..."}}`; the short form
/// is that message, behind any words before the JSON. Text with no such
/// message is its last line.
#[must_use]
pub fn summarize(text: &str) -> ErrorText {
    let full = text.trim().to_owned();
    let short = provider_message(&full)
        .unwrap_or_else(|| full.lines().last().unwrap_or_default().trim().to_owned());
    ErrorText {
        short: cut(&short),
        full,
    }
}

/// The error of `text` when it holds a provider's JSON message, and `None`
/// for any other text, which has no short form of its own.
#[must_use]
pub fn provider_error(text: &str) -> Option<ErrorText> {
    let full = text.trim().to_owned();
    let short = cut(&provider_message(&full)?);
    Some(ErrorText { short, full })
}

/// The message of the first JSON object in `text` that holds one, behind the
/// words that come before it on its line. A `{` that does not start JSON is
/// skipped, and the JSON may run over several lines.
fn provider_message(text: &str) -> Option<String> {
    text.match_indices('{').find_map(|(start, _)| {
        let value = serde_json::Deserializer::from_str(&text[start..])
            .into_iter::<Value>()
            .next()?
            .ok()?;
        let message = nested_message(&value, MAX_NESTING)?;
        let before = text[..start].rsplit('\n').next().unwrap_or_default().trim();
        Some(if before.is_empty() {
            message
        } else {
            format!("{} {message}", keep_end(before))
        })
    })
}

/// The last `MAX_PREFIX_CHARS` characters of `words`, marked when words were dropped.
fn keep_end(words: &str) -> String {
    let count = words.chars().count();
    if count <= MAX_PREFIX_CHARS {
        return words.to_owned();
    }
    let kept: String = words.chars().skip(count - MAX_PREFIX_CHARS + 1).collect();
    format!("\u{2026}{kept}")
}

/// The `error.message` or `message` of `value`. A message that is itself an
/// error object, as some gateways send, is followed.
fn nested_message(value: &Value, depth: usize) -> Option<String> {
    let message = [
        value.pointer("/error/message"),
        value.get("message"),
        value.get("error"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)?
    .trim();
    if message.is_empty() {
        return None;
    }
    if depth > 0
        && message.starts_with('{')
        && let Some(inner) = serde_json::from_str::<Value>(message)
            .ok()
            .and_then(|inner| nested_message(&inner, depth - 1))
    {
        return Some(inner);
    }
    Some(message.to_owned())
}

fn cut(text: &str) -> String {
    if text.chars().count() <= MAX_SHORT_CHARS {
        return text.to_owned();
    }
    let kept: String = text.chars().take(MAX_SHORT_CHARS - 1).collect();
    format!("{kept}\u{2026}")
}

/// The warning for a turn that ended at a plan limit. `remaining` is the time
/// to the reset the provider named, or `None` when it named none.
#[must_use]
pub fn usage_limit_text(window: UsageWindowKind, remaining: Option<Duration>) -> String {
    let window = match window {
        UsageWindowKind::FiveHour => Some(WINDOW_FIVE_HOUR),
        UsageWindowKind::SevenDay => Some(WINDOW_SEVEN_DAY),
        UsageWindowKind::ModelScoped => Some(WINDOW_MODEL_SCOPED),
        UsageWindowKind::Monthly => Some(WINDOW_MONTHLY),
        _ => None,
    };
    let head = window.map_or_else(
        || LIMIT_REACHED.to_owned(),
        |window| format!("{LIMIT_REACHED}: {window}"),
    );
    let tail = match remaining {
        None => RESET_UNKNOWN.to_owned(),
        Some(left) if left.is_zero() => RESET_PASSED.to_owned(),
        Some(left) => {
            let time = long_duration(left);
            if time.is_empty() {
                RESET_PASSED.to_owned()
            } else {
                format!("resets in {time}")
            }
        }
    };
    format!("{head}, {tail}")
}

/// `format_duration`, in days and hours from a day up.
pub(crate) fn long_duration(left: Duration) -> String {
    let seconds = left.as_secs();
    if seconds < SECONDS_PER_DAY {
        return format_duration(left);
    }
    format!(
        "{}d {}h",
        seconds / SECONDS_PER_DAY,
        seconds % SECONDS_PER_DAY / SECONDS_PER_HOUR
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_inside_a_providers_json_is_the_short_form() {
        // Arrange
        let text = r#"API Error: 400 {"type":"error","error":{"type":"invalid_request_error","message":"max_tokens is too large"}}"#;

        // Act
        let error = summarize(text);

        // Assert
        assert_eq!(error.short, "API Error: 400 max_tokens is too large");
        assert_eq!(error.full, text);
        assert!(error.expandable());
    }

    #[test]
    fn a_message_without_an_error_wrapper_and_one_nested_in_a_string_are_found() {
        // Arrange
        let flat = r#"{"message":"rate limited"}"#;
        let nested =
            r#"{"error":{"message":"{\"error\":{\"message\":\"the model is overloaded\"}}"}}"#;

        // Act
        let flat = summarize(flat);
        let nested = summarize(nested);

        // Assert
        assert_eq!(flat.short, "rate limited");
        assert_eq!(nested.short, "the model is overloaded");
    }

    #[test]
    fn only_the_last_line_counts_and_a_banner_above_it_stays_in_the_full_text() {
        // Arrange
        let text = "claude 2.1.296\nsomething odd happened\n";

        // Act
        let error = summarize(text);

        // Assert
        assert_eq!(error.short, "something odd happened");
        assert!(error.full.starts_with("claude 2.1.296"));
        assert!(error.expandable());
    }

    #[test]
    fn plain_one_line_text_has_nothing_to_expand() {
        // Act
        let error = summarize("connection refused");

        // Assert
        assert_eq!(error.short, "connection refused");
        assert!(!error.expandable());
    }

    #[test]
    fn braces_that_are_not_a_provider_error_leave_the_text_as_it_is() {
        for text in [
            "failed to parse { not json",
            r#"{"status":500}"#,
            r#"{"error":{"message":""}}"#,
            "{}",
        ] {
            // Act
            let error = summarize(text);

            // Assert
            assert_eq!(error.short, text, "{text}");
        }
    }

    #[test]
    fn a_long_message_is_cut_with_an_ellipsis_on_a_character_boundary() {
        // Arrange
        let text = format!(r#"{{"error":{{"message":"{}"}}}}"#, "é".repeat(500));

        // Act
        let error = summarize(&text);

        // Assert
        assert_eq!(error.short.chars().count(), MAX_SHORT_CHARS);
        assert!(error.short.ends_with('\u{2026}'));
    }

    #[test]
    fn a_pretty_printed_error_is_read_from_the_line_where_its_json_starts() {
        // Arrange
        let text =
            "request failed\n{\n  \"error\": {\n    \"message\": \"too many requests\"\n  }\n}";

        // Act
        let error = summarize(text);

        // Assert
        assert_eq!(error.short, "too many requests");
        assert!(error.expandable());
    }

    #[test]
    fn braces_before_the_json_and_a_message_that_is_not_text_do_not_hide_it() {
        // Arrange
        let braces = r#"failed {id} to call: {"error":{"message":"x"}}"#;
        let object_first = r#"{"error":{"message":{"text":"no"}},"message":"y"}"#;

        // Act
        let braces = summarize(braces);
        let object_first = summarize(object_first);

        // Assert
        assert_eq!(braces.short, "failed {id} to call: x");
        assert_eq!(object_first.short, "y");
    }

    #[test]
    fn a_long_prefix_is_cut_and_the_providers_message_stays() {
        // Arrange
        let text = format!(
            r#"{} {{"error":{{"message":"the real reason"}}}}"#,
            "p".repeat(500)
        );

        // Act
        let error = summarize(&text);

        // Assert
        assert!(error.short.ends_with("the real reason"), "{}", error.short);
        assert!(error.short.chars().count() < 200, "{}", error.short.len());
    }

    #[test]
    fn a_usage_limit_with_days_to_go_counts_in_days() {
        // Arrange
        let week = Duration::from_hours(168);

        // Act
        let text = usage_limit_text(UsageWindowKind::SevenDay, Some(week));

        // Assert
        assert_eq!(
            text,
            "usage limit reached: seven-day window, resets in 7d 0h"
        );
    }

    #[test]
    fn only_text_with_a_provider_message_has_a_provider_error() {
        // Act
        let wrapped = provider_error(r#"HTTP 429 {"error":{"message":"slow down"}}"#);
        let plain = provider_error("no repo\nsecond line");

        // Assert
        assert_eq!(wrapped.unwrap().short, "HTTP 429 slow down");
        assert_eq!(plain, None);
    }

    #[test]
    fn a_usage_limit_names_the_window_and_when_it_resets() {
        // Arrange
        let two_hours = Duration::from_mins(135);

        // Act
        let five = usage_limit_text(UsageWindowKind::FiveHour, Some(two_hours));
        let week = usage_limit_text(UsageWindowKind::SevenDay, Some(Duration::from_secs(90)));
        let unknown = usage_limit_text(UsageWindowKind::Unknown, None);
        let passed = usage_limit_text(UsageWindowKind::Monthly, Some(Duration::ZERO));

        // Assert
        assert_eq!(
            five,
            "usage limit reached: five-hour window, resets in 2h 15m"
        );
        assert_eq!(
            week,
            "usage limit reached: seven-day window, resets in 1m 30s"
        );
        assert_eq!(unknown, "usage limit reached, reset time unknown");
        assert_eq!(
            passed,
            "usage limit reached: monthly window, the window has reset"
        );
    }
}
