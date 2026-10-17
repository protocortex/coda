// SPDX-License-Identifier: AGPL-3.0-only

//! The one-line prompt the user types into.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const PROMPT: &str = "> ";

#[derive(Debug, Default)]
pub struct Input {
    text: String,
}

impl Input {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the finished line when Enter submits a non-blank one.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        match key.code {
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.text.push(ch);
            }
            KeyCode::Backspace => {
                self.text.pop();
            }
            KeyCode::Enter if !self.text.trim().is_empty() => {
                return Some(std::mem::take(&mut self.text));
            }
            _ => {}
        }
        None
    }

    /// Inserts pasted text as one line: newlines and tabs become spaces, so a
    /// paste can never submit, and other control characters are dropped.
    pub fn handle_paste(&mut self, pasted: &str) {
        let flattened = pasted.replace("\r\n", " ");
        self.text
            .extend(flattened.chars().filter_map(|ch| match ch {
                '\n' | '\r' | '\t' => Some(' '),
                ch if ch.is_control() => None,
                ch => Some(ch),
            }));
    }

    /// Draws the prompt and, when the line is too long, only its end.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let room = usize::from(area.width).saturating_sub(PROMPT.width());
        frame.render_widget(
            Paragraph::new(format!("{PROMPT}{}", tail_within(&self.text, room))),
            area,
        );
    }
}

/// The longest end of `text` that fits in `columns` display columns.
fn tail_within(text: &str, columns: usize) -> &str {
    let mut used = 0;
    let mut start = text.len();
    for (index, ch) in text.char_indices().rev() {
        used += ch.width().unwrap_or(0);
        if used > columns {
            break;
        }
        start = index;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};

    use super::*;
    use crate::test_support::screen;

    fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::NONE,
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::NONE, KeyEventKind::Press)
    }

    fn type_text(input: &mut Input, text: &str) {
        for ch in text.chars() {
            assert_eq!(input.handle_key(press(KeyCode::Char(ch))), None);
        }
    }

    fn draw(input: &Input, width: u16) -> String {
        screen(width, 1, |frame| input.render(frame, frame.area()))
    }

    #[test]
    fn typed_characters_build_the_line() {
        // Arrange
        let mut input = Input::new();

        // Act
        type_text(&mut input, "hi q");

        // Assert
        assert_eq!(input.text(), "hi q");
        assert_eq!(draw(&input, 10), "> hi q    ");
    }

    #[test]
    fn backspace_removes_the_last_character_and_is_safe_when_empty() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "a\u{4e2d}");

        // Act
        input.handle_key(press(KeyCode::Backspace));
        let after_one = input.text().to_owned();
        input.handle_key(press(KeyCode::Backspace));
        input.handle_key(press(KeyCode::Backspace));

        // Assert
        assert_eq!(after_one, "a");
        assert_eq!(input.text(), "");
    }

    #[test]
    fn enter_submits_the_line_and_clears_it() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "hello");

        // Act
        let submitted = input.handle_key(press(KeyCode::Enter));

        // Assert
        assert_eq!(submitted.as_deref(), Some("hello"));
        assert_eq!(input.text(), "");
    }

    #[test]
    fn enter_on_a_blank_line_submits_nothing() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "  ");

        // Act
        let submitted = input.handle_key(press(KeyCode::Enter));

        // Assert
        assert_eq!(submitted, None);
    }

    #[test]
    fn releases_and_control_chords_are_ignored() {
        // Arrange
        let mut input = Input::new();

        // Act
        input.handle_key(key(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        input.handle_key(key(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        ));

        // Assert
        assert_eq!(input.text(), "");
    }

    #[test]
    fn a_paste_inserts_text_with_newlines_as_spaces_and_never_submits() {
        // Arrange
        let mut input = Input::new();

        // Act
        input.handle_paste("one\ntwo\r\nthree\rfour\tfive");

        // Assert
        assert_eq!(input.text(), "one two three four five");
    }

    #[test]
    fn a_paste_drops_control_characters() {
        // Arrange
        let mut input = Input::new();

        // Act
        input.handle_paste("a\x1b[31mb\x07\u{9b}c\x7f");

        // Assert
        assert_eq!(input.text(), "a[31mbc");
    }

    #[test]
    fn a_long_line_shows_its_end_within_the_width() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "abcdefghij");

        // Act
        let shown = draw(&input, 6);

        // Assert
        assert_eq!(shown, "> ghij");
    }

    #[test]
    fn wide_characters_and_tiny_areas_do_not_panic() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "\u{4e2d}\u{6587}\u{4e2d}\u{6587}");

        // Act / Assert
        // A wide char fills two cells, which reads back with a blank between.
        let shown = draw(&input, 7).replace(' ', "");
        assert!(
            shown.starts_with('>') && shown.contains("\u{4e2d}\u{6587}"),
            "{shown}"
        );
        for width in [1, 2, 3] {
            let _ = draw(&input, width);
        }
        let _ = screen(0, 0, |frame| input.render(frame, frame.area()));
    }
}
