// SPDX-License-Identifier: Apache-2.0

//! The terminal sequences that tell the user the UI needs them while the
//! terminal is out of focus.
//!
//! No terminal code: this builds the bytes and the caller writes them.

use coda_config::tui::NotifyMode;

const BEL: char = '\u{7}';
const ESC: char = '\u{1b}';
const OSC9_PREFIX: &str = "]9;";
const OSC777_PREFIX: &str = "]777;notify;";
/// Longest title or body sent; terminals cut or refuse long ones.
const MAX_CHARS: usize = 100;

/// The sequence to write for a notification titled `title` with text `body`,
/// or `None` when `mode` is off.
#[must_use]
pub fn sequence(mode: NotifyMode, title: &str, body: &str) -> Option<String> {
    match mode {
        NotifyMode::Off => None,
        NotifyMode::Bell => Some(BEL.to_string()),
        NotifyMode::Osc9 => Some(format!(
            "{ESC}{OSC9_PREFIX}{}{BEL}",
            clean(&format!("{title}: {body}"))
        )),
        NotifyMode::Osc777 => Some(format!(
            "{ESC}{OSC777_PREFIX}{};{}{BEL}",
            clean(title),
            clean(body)
        )),
    }
}

/// Text that is safe inside a sequence: no control character, which would end
/// it early or start another, and no `;`, which splits the fields of OSC 777.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() || c == ';' { ' ' } else { c })
        .take(MAX_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_mode_writes_its_own_sequence_and_off_writes_nothing() {
        // Act
        let off = sequence(NotifyMode::Off, "coda", "done");
        let bell = sequence(NotifyMode::Bell, "coda", "done");
        let osc9 = sequence(NotifyMode::Osc9, "coda", "done");
        let osc777 = sequence(NotifyMode::Osc777, "coda", "done");

        // Assert
        assert_eq!(off, None);
        assert_eq!(bell.as_deref(), Some("\u{7}"));
        assert_eq!(osc9.as_deref(), Some("\u{1b}]9;coda: done\u{7}"));
        assert_eq!(osc777.as_deref(), Some("\u{1b}]777;notify;coda;done\u{7}"));
    }

    #[test]
    fn text_cannot_end_a_sequence_early_or_start_another() {
        // Arrange
        let hostile = "x\u{7}\u{1b}]0;owned\u{7};y\n";

        // Act
        let osc9 = sequence(NotifyMode::Osc9, "coda", hostile).unwrap();
        let osc777 = sequence(NotifyMode::Osc777, "coda", hostile).unwrap();

        // Assert
        for written in [&osc9, &osc777] {
            let inner = written
                .trim_start_matches('\u{1b}')
                .trim_end_matches('\u{7}');
            assert!(
                !inner.contains('\u{1b}') && !inner.contains('\u{7}'),
                "{written:?}"
            );
        }
        assert_eq!(osc777.matches(';').count(), 3);
    }

    #[test]
    fn a_long_body_is_cut() {
        // Arrange
        let body = "w".repeat(MAX_CHARS * 3);

        // Act
        let written = sequence(NotifyMode::Osc777, "coda", &body).unwrap();

        // Assert
        let expected = format!("\u{1b}]777;notify;coda;{}\u{7}", "w".repeat(MAX_CHARS));
        assert_eq!(written, expected);
    }
}
