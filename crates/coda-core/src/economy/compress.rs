// SPDX-License-Identifier: Apache-2.0

//! Deterministic, idempotent shrinkers for large tool output.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

/// Lines kept at the start of a log.
const LOG_HEAD_LINES: usize = 3;
/// Lines kept at the end of a log.
const LOG_TAIL_LINES: usize = 3;
/// Items kept at the start of a JSON array.
const JSON_HEAD_ITEMS: usize = 2;
/// Items kept at the end of a JSON array.
const JSON_TAIL_ITEMS: usize = 2;
/// Context lines kept on each side of a changed diff line.
const DIFF_CONTEXT_LINES: usize = 1;
/// Changed lines kept at each end of a run too long to keep whole.
const DIFF_RUN_KEEP_LINES: usize = 20;
/// Fewest lines a cut must leave out, so the marker is never longer than what it hides.
const DIFF_RUN_MIN_OMITTED: usize = 10;
/// Start of every omission marker, so a second pass can tell them from content.
const MARKER_PREFIX: &str = "[... ";
/// Lowercase words that make a line or item worth keeping.
const ERROR_WORDS: [&str; 5] = ["error", "warn", "fail", "panic", "fatal"];

pub(super) fn marker(count: usize, what: &str) -> String {
    format!("{MARKER_PREFIX}{count} {what} omitted]")
}

fn is_marker(line: &str) -> bool {
    line.starts_with(MARKER_PREFIX)
}

fn is_error_like(text: &str) -> bool {
    let lower = text.to_lowercase();
    ERROR_WORDS.iter().any(|word| lower.contains(word))
}

/// Keeps the items flagged in `keep`, replacing each omitted run with `marker(run_length)`.
fn collapse<T>(items: Vec<T>, keep: &[bool], marker: impl Fn(usize) -> T) -> Vec<T> {
    let mut out = Vec::with_capacity(items.len());
    let mut omitted = 0;
    for (item, &kept) in items.into_iter().zip(keep) {
        if kept {
            if omitted > 0 {
                out.push(marker(omitted));
                omitted = 0;
            }
            out.push(item);
        } else {
            omitted += 1;
        }
    }
    if omitted > 0 {
        out.push(marker(omitted));
    }
    out
}

/// Joins lines with newlines, ending with one when the source did.
fn join_lines(lines: &[String], source: &str) -> String {
    let mut out = lines.join("\n");
    if source.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Shrinks a log, keeping its head, tail and every error-like line.
///
/// Invalid UTF-8 is replaced with U+FFFD.
#[must_use]
pub fn compress_log(input: &[u8]) -> String {
    let text = String::from_utf8_lossy(input);
    let lines: Vec<&str> = text.lines().collect();
    let last = lines.len();
    let keep: Vec<bool> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            i < LOG_HEAD_LINES
                || i + LOG_TAIL_LINES >= last
                || is_error_like(line)
                || is_marker(line)
        })
        .collect();
    let owned = lines.into_iter().map(str::to_owned).collect();
    let kept = collapse(owned, &keep, |n| marker(n, "lines"));
    join_lines(&kept, &text)
}

/// Shrinks a unified diff, keeping every header and hunk header.
///
/// Context lines survive only next to a change. A run of changed lines that
/// has `2 * DIFF_RUN_KEEP_LINES + DIFF_RUN_MIN_OMITTED` lines or more keeps its
/// first and last `DIFF_RUN_KEEP_LINES` lines, so a rewritten file does not fill
/// the context. Invalid UTF-8 is replaced with U+FFFD.
#[must_use]
pub fn compress_diff(input: &[u8]) -> String {
    let text = String::from_utf8_lossy(input);
    let lines: Vec<&str> = text.lines().collect();
    let in_hunk = hunk_flags(&lines);
    let is_context = |i: usize| in_hunk[i] && (lines[i].is_empty() || lines[i].starts_with(' '));
    let is_change = |i: usize| in_hunk[i] && !is_context(i) && !lines[i].starts_with("@@");
    let mut keep: Vec<bool> = (0..lines.len())
        .map(|i| {
            if !is_context(i) {
                return true;
            }
            let from = i.saturating_sub(DIFF_CONTEXT_LINES);
            let to = (i + DIFF_CONTEXT_LINES).min(lines.len() - 1);
            (from..=to).any(|near| near != i && !is_context(near) && !lines[near].starts_with("@@"))
        })
        .collect();
    let mut start = 0;
    while start < lines.len() {
        if !is_change(start) {
            start += 1;
            continue;
        }
        let end = (start..lines.len())
            .find(|&i| !is_change(i))
            .unwrap_or(lines.len());
        if end - start >= 2 * DIFF_RUN_KEEP_LINES + DIFF_RUN_MIN_OMITTED {
            keep[start + DIFF_RUN_KEEP_LINES..end - DIFF_RUN_KEEP_LINES].fill(false);
        }
        start = end;
    }
    let mut kept = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if keep[i] {
            kept.push(lines[i].to_owned());
            i += 1;
            continue;
        }
        let first = i;
        while i < lines.len() && !keep[i] {
            i += 1;
        }
        if is_context(first) {
            kept.push(marker(i - first, "context lines"));
        } else {
            kept.push(changed_marker(&lines[first..i]));
        }
    }
    join_lines(&kept, &text)
}

/// The marker for a cut run of changed lines, which says how many were removed and added.
fn changed_marker(omitted: &[&str]) -> String {
    let count = |sign: char| omitted.iter().filter(|line| line.starts_with(sign)).count();
    format!(
        "{MARKER_PREFIX}{} changed lines omitted: {} removed, {} added]",
        omitted.len(),
        count('-'),
        count('+')
    )
}

/// Flags the lines that follow a hunk header, where a leading space means context.
fn hunk_flags(lines: &[&str]) -> Vec<bool> {
    let mut inside = false;
    lines
        .iter()
        .map(|line| {
            if line.starts_with("@@") {
                inside = true;
            } else if line.starts_with("diff ") {
                inside = false;
            }
            inside
        })
        .collect()
}

/// A JSON value parsed without any loss: floats, exponents, integers beyond
/// 64 bits and duplicate object keys are errors instead of being rewritten.
struct Exact(Value);

impl<'de> Deserialize<'de> for Exact {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ExactVisitor;

        impl<'de> Visitor<'de> for ExactVisitor {
            type Value = Value;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON whose values survive a rewrite unchanged")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
                Ok(Value::Bool(v))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
                Ok(v.into())
            }

            fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
                Ok(v.into())
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Value, E> {
                Err(E::custom("number would not round-trip"))
            }

            fn visit_str<E>(self, v: &str) -> Result<Value, E> {
                Ok(Value::String(v.to_owned()))
            }

            fn visit_unit<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
                let mut items = Vec::new();
                while let Some(Exact(item)) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Value::Array(items))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
                let mut out = Map::new();
                while let Some((key, Exact(value))) = map.next_entry::<String, Exact>()? {
                    if out.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate key"));
                    }
                }
                Ok(Value::Object(out))
            }
        }

        deserializer.deserialize_any(ExactVisitor).map(Exact)
    }
}

/// Shrinks a JSON array, or returns `None` when the input is not one or
/// cannot be rewritten without altering a value.
///
/// Keeps the first and last items and every error-like item, and writes
/// compact JSON with sorted keys.
#[must_use]
pub fn compress_json_array(input: &[u8]) -> Option<String> {
    let Ok(Exact(Value::Array(items))) = serde_json::from_slice(input) else {
        return None;
    };
    let last = items.len();
    let keep: Vec<bool> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            i < JSON_HEAD_ITEMS
                || i + JSON_TAIL_ITEMS >= last
                || item.as_str().is_some_and(is_marker)
                || is_error_like(&item.to_string())
        })
        .collect();
    let kept = collapse(items, &keep, |n| Value::String(marker(n, "items")));
    serde_json::to_string(&kept).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    const LOG: &str = "l1\nl2\nl3\nl4\nl5\nl6\nl7 error: boom\nl8\nl9\nl10\nl11\nl12\n";
    const LOG_COMPRESSED: &str = "l1\nl2\nl3\n[... 3 lines omitted]\nl7 error: boom\n\
[... 2 lines omitted]\nl10\nl11\nl12\n";

    const DIFF: &str = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1,9 +1,9 @@\n\
\x20c1\n c2\n c3\n-old\n+new\n c4\n c5\n c6\n";
    const DIFF_COMPRESSED: &str = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n\
@@ -1,9 +1,9 @@\n[... 2 context lines omitted]\n c3\n-old\n+new\n c4\n\
[... 2 context lines omitted]\n";

    const JSON: &str = r#"[{"id": 1}, {"id": 2}, {"id": 3}, {"id": 4},
        {"id": 5, "error": "boom"}, {"id": 6}, {"id": 7}, {"id": 8}]"#;
    const JSON_COMPRESSED: &str = r#"[{"id":1},{"id":2},"[... 2 items omitted]",{"error":"boom","id":5},"[... 1 items omitted]",{"id":7},{"id":8}]"#;

    #[test]
    fn log_keeps_head_tail_and_error_lines() {
        // Arrange
        let input = LOG.as_bytes();

        // Act
        let out = compress_log(input);

        // Assert
        assert_eq!(out, LOG_COMPRESSED);
    }

    #[test]
    fn diff_keeps_headers_hunks_and_changes() {
        // Arrange
        let input = DIFF.as_bytes();

        // Act
        let out = compress_diff(input);

        // Assert
        assert_eq!(out, DIFF_COMPRESSED);
    }

    fn long_added_file(lines: usize) -> String {
        let body = (1..=lines).fold(String::new(), |mut body, n| {
            let _ = writeln!(body, "+line {n}");
            body
        });
        format!("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -0,0 +1,{lines} @@\n{body}")
    }

    #[test]
    fn diff_cuts_the_middle_of_a_long_run_of_changes_and_keeps_both_ends() {
        // Arrange
        let input = long_added_file(100);

        // Act
        let out = compress_diff(input.as_bytes());

        // Assert
        assert!(out.contains("+line 1\n"), "{out}");
        assert!(out.contains("+line 20\n"), "{out}");
        assert!(
            out.contains("[... 60 changed lines omitted: 0 removed, 60 added]\n"),
            "{out}"
        );
        assert!(out.contains("+line 81\n"), "{out}");
        assert!(out.ends_with("+line 100\n"), "{out}");
        assert!(!out.contains("+line 21\n"), "{out}");
    }

    #[test]
    fn diff_keeps_a_run_of_changes_that_is_not_long() {
        // Arrange
        let input = long_added_file(49);

        // Act
        let out = compress_diff(input.as_bytes());

        // Assert
        assert_eq!(out, input);
    }

    #[test]
    fn the_marker_of_a_cut_run_counts_removed_and_added_lines() {
        // Arrange
        let lines = |sign: char| {
            (1..=30).fold(String::new(), |mut text, n| {
                let _ = writeln!(text, "{sign}line {n}");
                text
            })
        };
        let (removed, added) = (lines('-'), lines('+'));
        let input =
            format!("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,30 +1,30 @@\n{removed}{added}");

        // Act
        let out = compress_diff(input.as_bytes());

        // Assert
        assert!(
            out.contains("[... 20 changed lines omitted: 10 removed, 10 added]\n"),
            "{out}"
        );
    }

    #[test]
    fn compressing_a_diff_with_a_cut_run_twice_changes_nothing() {
        // Arrange
        let once = compress_diff(long_added_file(100).as_bytes());

        // Act
        let twice = compress_diff(once.as_bytes());

        // Assert
        assert_eq!(twice, once);
    }

    #[test]
    fn json_array_keeps_head_tail_and_error_items() {
        // Arrange
        let input = JSON.as_bytes();

        // Act
        let out = compress_json_array(input);

        // Assert
        assert_eq!(out.as_deref(), Some(JSON_COMPRESSED));
    }

    #[test]
    fn compressing_twice_changes_nothing() {
        // Arrange
        let log = compress_log(LOG.as_bytes());
        let diff = compress_diff(DIFF.as_bytes());
        let json = compress_json_array(JSON.as_bytes()).unwrap();

        // Act
        let again = (
            compress_log(log.as_bytes()),
            compress_diff(diff.as_bytes()),
            compress_json_array(json.as_bytes()).unwrap(),
        );

        // Assert
        assert_eq!(again, (log, diff, json));
    }

    #[test]
    fn compressing_the_same_input_twice_gives_the_same_output() {
        // Arrange
        let input = LOG.as_bytes();

        // Act
        let (first, second) = (compress_log(input), compress_log(input));

        // Assert
        assert_eq!(first, second);
    }

    #[test]
    fn empty_input_stays_empty() {
        // Arrange
        let empty = b"";

        // Act
        let out = (
            compress_log(empty),
            compress_diff(empty),
            compress_json_array(empty),
        );

        // Assert
        assert_eq!(out, (String::new(), String::new(), None));
    }

    #[test]
    fn a_single_line_is_kept_whole() {
        // Arrange
        let line = b"only line, no newline";

        // Act
        let (log, diff) = (compress_log(line), compress_diff(line));

        // Assert
        assert_eq!(
            (log.as_str(), diff.as_str()),
            ("only line, no newline", "only line, no newline")
        );
    }

    #[test]
    fn invalid_utf8_is_replaced_not_rejected() {
        // Arrange
        let input = b"ok\xff\n";

        // Act
        let out = compress_log(input);

        // Assert
        assert_eq!(out, "ok\u{fffd}\n");
    }

    #[test]
    fn json_that_is_not_an_array_is_declined() {
        // Arrange
        let inputs: [&[u8]; 3] = [b"{\"a\":1}", b"[1,", b"\xff"];

        // Act
        let out: Vec<_> = inputs.into_iter().map(compress_json_array).collect();

        // Assert
        assert_eq!(out, [None, None, None]);
    }

    #[test]
    fn json_with_values_that_would_change_is_declined() {
        // Arrange
        let inputs: [&[u8]; 5] = [
            b"[99999999999999999999, 2]",
            b"[1e2, 2]",
            b"[1.5, 2]",
            br#"[{"a": 1, "a": 2}]"#,
            b"[-0, 2]",
        ];

        // Act
        let out: Vec<_> = inputs.into_iter().map(compress_json_array).collect();

        // Assert
        assert_eq!(out, vec![None; 5]);
    }

    #[test]
    fn json_integers_in_range_survive() {
        // Arrange
        let input = b"[18446744073709551615, -9223372036854775808, null, \"s\"]";

        // Act
        let out = compress_json_array(input);

        // Assert
        assert_eq!(
            out.as_deref(),
            Some("[18446744073709551615,-9223372036854775808,null,\"s\"]")
        );
    }
}
