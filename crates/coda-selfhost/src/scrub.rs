// SPDX-License-Identifier: AGPL-3.0-only

//! Removes environment values, absolute paths and identifiers from a transcript.

use std::fmt::Write as _;

/// Shorter values are skipped: they would match unrelated text.
const MIN_SECRET_LEN: usize = 6;
const PATH_PLACEHOLDER: &str = "<path>";
const KEY_PLACEHOLDER: &str = "<key>";
const KEY_PREFIX: &str = "sk-";
const EMAIL_PLACEHOLDER: &str = "<email>";
const OPAQUE_PLACEHOLDER: &str = "<opaque>";
const UUID_LEN: usize = 36;
/// Positions of the dashes in `8-4-4-4-12` hex groups.
const UUID_DASHES: [usize; 4] = [8, 13, 18, 23];
/// Shorter runs are ordinary words, however mixed their characters.
const MIN_OPAQUE_LEN: usize = 40;
/// An id such as `msg_01AbC...` has a short lowercase prefix and a long body.
const MAX_ID_PREFIX_LEN: usize = 8;
const MIN_ID_BODY_LEN: usize = 16;
/// Shorter tokens that start with the prefix are ordinary words.
const MIN_KEY_LEN: usize = 20;
const NUMBER_PLACEHOLDER: &str = "0";
const REDACTED_PLACEHOLDER: &str = "<redacted>";
/// JSON fields that describe the account or its quota, not the protocol.
const QUOTA_NUMBER_FIELDS: [&str; 5] = [
    "usedPercent",
    "utilization",
    "resetsAt",
    "windowDurationMins",
    "balance",
];
const ACCOUNT_STRING_FIELDS: [&str; 4] = [
    "authMode",
    "planType",
    "overageStatus",
    "overageDisabledReason",
];
const USER_AGENT_FIELD: &str = "userAgent";
const USER_AGENT_PLATFORM: &str = "<platform>";

/// Replaces every long enough environment value and every absolute path in `text`.
#[must_use]
pub fn scrub(text: &str, env: &[(String, String)]) -> String {
    let mut secrets: Vec<&(String, String)> = env
        .iter()
        .filter(|(_, value)| value.len() >= MIN_SECRET_LEN)
        .collect();
    // Longest first, so a value that contains another is replaced whole.
    secrets.sort_by_key(|(_, value)| std::cmp::Reverse(value.len()));
    let mut out = text.to_owned();
    for (name, value) in secrets {
        out = out.replace(value, &format!("<env:{name}>"));
    }
    let out = scrub_account_fields(&out);
    let out = scrub_paths(&scrub_keys(&out));
    scrub_opaque(&scrub_uuids(&scrub_emails(&out)))
}

/// The 1-based number of the first line of `text` that is not a JSON value.
///
/// Scrubbing works on text, so a bad replacement could break a line.
#[must_use]
pub fn first_invalid_json_line(text: &str) -> Option<usize> {
    text.lines()
        .position(|line| serde_json::from_str::<serde_json::Value>(line).is_err())
        .map(|index| index + 1)
}

/// The end of the JSON scalar at the start of `value`, if it is a string or number.
fn scalar_len(value: &str) -> Option<usize> {
    if let Some(body) = value.strip_prefix('"') {
        let mut escaped = false;
        for (i, c) in body.char_indices() {
            match (escaped, c) {
                (false, '"') => return Some(i + 2),
                (false, '\\') => escaped = true,
                _ => escaped = false,
            }
        }
        return None;
    }
    let len = value
        .find(|c: char| !(c.is_ascii_digit() || "+-.eE".contains(c)))
        .unwrap_or(value.len());
    (len > 0).then_some(len)
}

/// Replaces the scalar after each `"field":` with `placeholder(scalar)`, when
/// that returns one. Other values of the field (null, objects, booleans) stay.
fn redact_field(text: &str, field: &str, placeholder: impl Fn(&str) -> Option<String>) -> String {
    let needle = format!("\"{field}\":");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(&needle) {
        let value_at = at + needle.len();
        out.push_str(&rest[..value_at]);
        rest = &rest[value_at..];
        if let Some(len) = scalar_len(rest)
            && let Some(replacement) = placeholder(&rest[..len])
        {
            out.push_str(&replacement);
            rest = &rest[len..];
        }
    }
    out.push_str(rest);
    out
}

/// Drops account and quota values (plan, login mode, usage, reset times) and the
/// operating system and terminal named in a `userAgent`.
fn scrub_account_fields(text: &str) -> String {
    let mut out = text.to_owned();
    for field in QUOTA_NUMBER_FIELDS {
        out = redact_field(&out, field, |v| {
            (!v.starts_with('"')).then(|| NUMBER_PLACEHOLDER.to_owned())
        });
    }
    for field in ACCOUNT_STRING_FIELDS {
        out = redact_field(&out, field, |v| {
            v.starts_with('"')
                .then(|| format!("\"{REDACTED_PLACEHOLDER}\""))
        });
    }
    redact_field(&out, USER_AGENT_FIELD, |v| {
        let product = v.trim_matches('"').split(' ').next().unwrap_or_default();
        Some(format!("\"{product} {USER_AGENT_PLATFORM}\""))
    })
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Replaces API keys shaped like `sk-...` and `sk-ant-...`.
fn scrub_keys(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(KEY_PREFIX) {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let len = tail.find(|c| !is_key_char(c)).unwrap_or(tail.len());
        if len >= MIN_KEY_LEN && out.chars().next_back().is_none_or(|c| !is_key_char(c)) {
            out.push_str(KEY_PLACEHOLDER);
            rest = &tail[len..];
        } else {
            out.push_str(KEY_PREFIX);
            rest = &tail[KEY_PREFIX.len()..];
        }
    }
    out.push_str(rest);
    out
}

fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || "._-+@~/".contains(c)
}

/// Whether the `/` after `before` is the root slash of a `file:///` URL.
fn is_file_url_root(before: &[char]) -> bool {
    before.iter().rev().take(7).rev().collect::<String>() == "file://"
}

/// Replaces each `/...` run that starts a token. A `/` that follows a path
/// character belongs to a relative path or a URL, and `//` starts neither.
fn scrub_paths(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let starts_path = chars[i] == '/'
            && (i == 0 || !is_path_char(chars[i - 1]) || is_file_url_root(&chars[..i]))
            && chars
                .get(i + 1)
                .is_some_and(|&c| is_path_char(c) && c != '/');
        if !starts_path {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut end = i;
        while end < chars.len() && is_path_char(chars[end]) {
            end += 1;
        }
        // Sentence punctuation after a path is not part of it.
        while chars[end - 1] == '.' {
            end -= 1;
        }
        out.push_str(PATH_PLACEHOLDER);
        i = end;
    }
    out
}

fn is_email_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "._%+-".contains(c)
}

/// Replaces `local@domain.tld` addresses. A domain without a dot is not one.
fn scrub_emails(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        let (head, tail) = (&rest[..at], &rest[at + 1..]);
        let local_len = head.chars().rev().take_while(|&c| is_email_char(c)).count();
        let domain_len = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
            .unwrap_or(tail.len());
        let domain = tail[..domain_len].trim_end_matches(['.', '-']);
        if local_len > 0 && domain.contains('.') {
            out.push_str(&head[..head.len() - local_len]);
            out.push_str(EMAIL_PLACEHOLDER);
            rest = &tail[domain.len()..];
        } else {
            out.push_str(&rest[..=at]);
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn is_uuid_at(bytes: &[u8]) -> bool {
    bytes.len() >= UUID_LEN
        && bytes[..UUID_LEN].iter().enumerate().all(|(i, &b)| {
            if UUID_DASHES.contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

/// Replaces each distinct UUID with `00000000-0000-4000-8000-<n>`, numbered in
/// order of appearance, so a reader can still tell which ids are the same.
fn scrub_uuids(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut seen: Vec<&str> = Vec::new();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let bounded_before = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let bounded_after = bytes
            .get(i + UUID_LEN)
            .is_none_or(|b| !b.is_ascii_alphanumeric());
        if bounded_before && is_uuid_at(&bytes[i..]) && bounded_after {
            let id = &text[i..i + UUID_LEN];
            let n = seen.iter().position(|s| *s == id).unwrap_or_else(|| {
                seen.push(id);
                seen.len() - 1
            }) + 1;
            let _ = write!(out, "00000000-0000-4000-8000-{n:012}");
            i += UUID_LEN;
        } else {
            let len = text[i..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&text[i..i + len]);
            i += len;
        }
    }
    out
}

fn is_blob_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_-+/=".contains(c)
}

fn is_mixed(run: &str) -> bool {
    let has = |test: fn(&char) -> bool| run.chars().any(|c| test(&c));
    has(char::is_ascii_digit) && has(char::is_ascii_uppercase) && has(char::is_ascii_lowercase)
}

/// The run as a `prefix_body` id, such as `msg_01AbC...`, or `None`.
fn id_prefix(run: &str) -> Option<&str> {
    let (prefix, body) = run.split_once('_')?;
    let plain_prefix = (2..=MAX_ID_PREFIX_LEN).contains(&prefix.len())
        && prefix.bytes().all(|b| b.is_ascii_lowercase());
    let opaque_body = body.len() >= MIN_ID_BODY_LEN
        && body.bytes().all(|b| b.is_ascii_alphanumeric())
        && body.bytes().any(|b| b.is_ascii_digit());
    (plain_prefix && opaque_body).then_some(prefix)
}

/// Replaces long mixed-case runs (signatures, encoded blobs) with `<opaque>`
/// and `prefix_body` ids with `prefix_<n>`, numbered across distinct ids.
fn scrub_opaque(text: &str) -> String {
    let mut ids: Vec<&str> = Vec::new();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let blob = rest.starts_with(is_blob_char);
        let len = rest.find(|c| is_blob_char(c) != blob).unwrap_or(rest.len());
        let run = &rest[..len];
        rest = &rest[len..];
        if !blob {
            out.push_str(run);
        } else if run.len() >= MIN_OPAQUE_LEN && is_mixed(run) {
            out.push_str(OPAQUE_PLACEHOLDER);
        } else if let Some(prefix) = id_prefix(run) {
            let n = ids.iter().position(|s| *s == run).unwrap_or_else(|| {
                ids.push(run);
                ids.len() - 1
            }) + 1;
            let _ = write!(out, "{prefix}_{n}");
        } else {
            out.push_str(run);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_that_is_not_json_is_reported_by_its_number() {
        // Arrange
        let text = "{\"a\":1}\n{\"b\":\n[2]\n";

        // Act
        let invalid = first_invalid_json_line(text);

        // Assert
        assert_eq!(invalid, Some(2));
    }

    #[test]
    fn text_with_only_json_lines_has_no_invalid_line() {
        // Act
        let invalid = first_invalid_json_line("{\"a\":1}\n\"x\"\n");

        // Assert
        assert_eq!(invalid, None);
    }

    #[test]
    fn environment_values_are_replaced_longest_first_and_short_ones_kept() {
        // Arrange
        let vars = vec![
            ("API_KEY".to_owned(), "canary-7f3a9c".to_owned()),
            ("SHORT".to_owned(), "secret".to_owned()),
            ("LONG".to_owned(), "secret-suffix".to_owned()),
            ("DEPTH".to_owned(), "1".to_owned()),
        ];

        // Act
        let scrubbed = scrub(
            "canary-7f3a9c x canary-7f3a9c a secret-suffix b level 1",
            &vars,
        );

        // Assert
        assert_eq!(
            scrubbed,
            "<env:API_KEY> x <env:API_KEY> a <env:LONG> b level 1"
        );
    }

    #[test]
    fn absolute_paths_are_removed_but_relative_paths_and_urls_are_kept() {
        // Arrange
        let text = r#"edit /Users/jo/repo/main.rs in crates/a/main.rs, see ./a/b and /tmp/x. {"cwd":"/private/tmp/x1"} HOME=/home/a https://example.com/v1/x lone / slash"#;

        // Act
        let scrubbed = scrub(text, &[]);

        // Assert
        assert_eq!(
            scrubbed,
            r#"edit <path> in crates/a/main.rs, see ./a/b and <path>. {"cwd":"<path>"} HOME=<path> https://example.com/v1/x lone / slash"#
        );
    }

    #[test]
    fn file_urls_and_api_keys_are_removed() {
        // Act
        let scrubbed = scrub(
            "open file:///Users/jo/x.rs with sk-ant-api03-AbC_123-xyzxyzxyz and sk-proj1234567890abcdefgh, not my-task-aaaaaaaaaaaaaaaaaaaaaaaa or sk-short",
            &[],
        );

        // Assert
        assert_eq!(
            scrubbed,
            "open file://<path> with <key> and <key>, not my-task-aaaaaaaaaaaaaaaaaaaaaaaa or sk-short"
        );
    }

    #[test]
    fn emails_are_removed_but_scoped_names_are_kept() {
        // Act
        let scrubbed = scrub(
            r#"{"who":"jo.doe+x@example.co.uk,","plugin":"agents-md@builtin","at":"a @ b"}"#,
            &[],
        );

        // Assert
        assert_eq!(
            scrubbed,
            r#"{"who":"<email>,","plugin":"agents-md@builtin","at":"a @ b"}"#
        );
    }

    #[test]
    fn uuids_get_stable_numbered_placeholders_in_order_of_appearance() {
        // Arrange
        let a = "0123abcd-4567-89ab-cdef-0123456789ab";
        let b = "FEDCBA98-7654-3210-fedc-ba9876543210";

        // Act
        let scrubbed = scrub(&format!("{a} {b} {a} x{a} 1234-5678"), &[]);

        // Assert
        let one = "00000000-0000-4000-8000-000000000001";
        let two = "00000000-0000-4000-8000-000000000002";
        assert_eq!(scrubbed, format!("{one} {two} {one} x{a} 1234-5678"));
    }

    #[test]
    fn prefixed_ids_and_long_mixed_blobs_are_replaced_and_words_are_kept() {
        // Arrange
        let blob = "EsQBCkYIAxgCKkAh9Zk3QmN7vWqXyT2pLcRdUeFgHjKiOaSbVnMz81xw";
        let text = format!(
            r#"msg_01AbCdEfGhIjKlMnOp toolu_01AbCdEfGhIjKlMnOp msg_01AbCdEfGhIjKlMnOp "signature":"{blob}" cache_creation_input_tokens interrupt_cancel_queued_v1 item_0"#
        );

        // Act
        let scrubbed = scrub(&text, &[]);

        // Assert
        assert_eq!(
            scrubbed,
            r#"msg_1 toolu_2 msg_1 "signature":"<opaque>" cache_creation_input_tokens interrupt_cancel_queued_v1 item_0"#
        );
    }

    #[test]
    fn account_and_quota_fields_are_redacted_and_protocol_fields_are_kept() {
        // Arrange
        let text = concat!(
            r#"{"authMode":"chatgpt","planType":"free","rateLimits":{"credits":{"balance":"12.5","hasCredits":false},"#,
            r#""primary":{"usedPercent":8,"resetsAt":1794957349,"windowDurationMins":43200},"secondary":null},"#,
            r#""utilization":0.04,"overageStatus":"rejected","overageDisabledReason":"org_level_disabled_until","status":"allowed","used":3,"#,
            r#""userAgent":"coda/0.163.0 (Mac OS 15.8.1; arm64) tmux-256color (coda; 0.0.0)"}"#
        );

        // Act
        let scrubbed = scrub(text, &[]);

        // Assert
        assert_eq!(
            scrubbed,
            concat!(
                r#"{"authMode":"<redacted>","planType":"<redacted>","rateLimits":{"credits":{"balance":"12.5","hasCredits":false},"#,
                r#""primary":{"usedPercent":0,"resetsAt":0,"windowDurationMins":0},"secondary":null},"#,
                r#""utilization":0,"overageStatus":"<redacted>","overageDisabledReason":"<redacted>","status":"allowed","used":3,"#,
                r#""userAgent":"coda/0.163.0 <platform>"}"#
            )
        );
    }
}
