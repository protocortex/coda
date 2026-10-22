// SPDX-License-Identifier: AGPL-3.0-only

//! Splits a definition file into its TOML frontmatter and its markdown body.

/// The line that opens and closes the frontmatter.
const FENCE: &str = "+++";
const BYTE_ORDER_MARK: char = '\u{feff}';

/// Returns the frontmatter text and the body, or `None` when the file does not
/// start with a fence line or never closes it.
pub(crate) fn split(text: &str) -> Option<(&str, &str)> {
    // Editors on Windows prepend a byte order mark; it is not part of the text.
    let text = text.strip_prefix(BYTE_ORDER_MARK).unwrap_or(text);
    let rest = strip_fence_line(text)?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == FENCE {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

fn strip_fence_line(text: &str) -> Option<&str> {
    let first = text.split_inclusive('\n').next()?;
    (first.trim_end_matches(['\r', '\n']) == FENCE).then(|| &text[first.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_frontmatter_from_body() {
        assert_eq!(
            split("+++\na = 1\n+++\nbody\n"),
            Some(("a = 1\n", "body\n"))
        );
    }

    #[test]
    fn a_leading_byte_order_mark_is_stripped() {
        assert_eq!(split("\u{feff}+++\na = 1\n+++\nx"), Some(("a = 1\n", "x")));
    }

    #[test]
    fn an_empty_body_is_allowed() {
        assert_eq!(split("+++\na = 1\n+++"), Some(("a = 1\n", "")));
    }

    #[test]
    fn a_missing_open_or_close_fence_is_none() {
        assert_eq!(split("a = 1\n+++\n"), None);
        assert_eq!(split("+++\na = 1\n"), None);
        assert_eq!(split(""), None);
    }

    #[test]
    fn a_fence_like_line_inside_the_body_stays_in_the_body() {
        assert_eq!(split("+++\n+++\nx\n+++\n"), Some(("", "x\n+++\n")));
    }
}
