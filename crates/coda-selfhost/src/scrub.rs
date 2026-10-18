// SPDX-License-Identifier: AGPL-3.0-only

//! Removes environment values and absolute paths from a transcript.

/// Shorter values are skipped: they would match unrelated text.
const MIN_SECRET_LEN: usize = 6;
const PATH_PLACEHOLDER: &str = "<path>";
const KEY_PLACEHOLDER: &str = "<key>";
const KEY_PREFIX: &str = "sk-";
/// Shorter tokens that start with the prefix are ordinary words.
const MIN_KEY_LEN: usize = 20;

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
    scrub_paths(&scrub_keys(&out))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
