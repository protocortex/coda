// SPDX-License-Identifier: Apache-2.0

//! The prompt the user types into: several lines, a cursor, recall of earlier
//! prompts and a stash for a half-written one.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use std::cell::Cell;

use ratatui::layout::{Position, Rect};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const PROMPT: &str = "> ";
const CONTINUATION: &str = "  ";
/// The most rows the prompt takes before it scrolls.
pub const MAX_ROWS: u16 = 6;
/// How many earlier prompts Up can bring back.
const MAX_HISTORY: usize = 100;
/// The columns for text before the first draw tells the real width.
const DEFAULT_ROOM: usize = 78;

/// What a stash request did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stashed {
    /// The prompt went to the stash and the line is empty.
    Saved,
    /// The newest stashed prompt is back in the line.
    Restored,
    /// The line was empty and nothing was stashed.
    Nothing,
}

/// One drawn row of the prompt: a byte range of the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Row {
    start: usize,
    end: usize,
}

#[derive(Debug)]
pub struct Input {
    text: String,
    /// Byte offset of the cursor, always on a character boundary.
    cursor: usize,
    history: Vec<String>,
    /// The history entry the line shows, while the line has not been edited.
    recall: Option<usize>,
    stash: Vec<String>,
    /// The columns the last draw had for text, which Up, Down, Home and End
    /// need to follow the drawn rows.
    room: Cell<usize>,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            history: Vec::new(),
            recall: None,
            stash: Vec::new(),
            room: Cell::new(DEFAULT_ROOM),
        }
    }
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

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.recall = None;
    }

    /// Clears the line, first keeping a draft the user wrote in the stash.
    /// Returns whether a draft went to the stash.
    pub fn discard(&mut self) -> bool {
        let kept = self.showing_recall().is_none() && !self.text.trim().is_empty();
        if kept {
            self.push_stash(self.text.clone());
        }
        self.clear();
        kept
    }

    fn push_stash(&mut self, text: String) {
        self.stash.push(text);
        if self.stash.len() > MAX_HISTORY {
            self.stash.remove(0);
        }
    }

    /// Puts back text the user had typed, as it was.
    pub fn restore(&mut self, typed: &str) {
        typed.clone_into(&mut self.text);
        self.cursor = self.text.len();
        self.recall = None;
    }

    /// Returns the finished line when Enter submits a non-blank one.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert("\n");
            }
            KeyCode::Enter if !self.text.trim().is_empty() => return Some(self.submit()),
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::Char(ch) if !ctrl && !alt => self.insert(ch.encode_utf8(&mut [0; 4])),
            KeyCode::Char(ch) if ctrl => self.control(ch),
            KeyCode::Char(ch) if alt => self.alt(ch),
            KeyCode::Backspace if alt => self.delete_word_back(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left if ctrl || alt => self.cursor = self.word_start(),
            KeyCode::Right if ctrl || alt => self.cursor = self.word_end(),
            KeyCode::Left => self.cursor = self.previous_boundary(),
            KeyCode::Right => self.cursor = self.next_boundary(),
            KeyCode::Home => self.cursor = self.row_bounds().0,
            KeyCode::End => self.cursor = self.row_bounds().1,
            KeyCode::Up => self.up(),
            KeyCode::Down => self.down(),
            _ => {}
        }
        None
    }

    fn control(&mut self, ch: char) {
        match ch {
            'a' => self.cursor = self.row_bounds().0,
            'b' => self.cursor = self.previous_boundary(),
            'f' => self.cursor = self.next_boundary(),
            'w' => self.delete_word_back(),
            'u' => self.delete_to_line_start(),
            _ => {}
        }
    }

    fn alt(&mut self, ch: char) {
        match ch {
            'b' => self.cursor = self.word_start(),
            'f' => self.cursor = self.word_end(),
            _ => {}
        }
    }

    /// Takes the line, remembers it for recall and empties the prompt.
    fn submit(&mut self) -> String {
        let taken = std::mem::take(&mut self.text);
        let line = taken.trim_start_matches(['\n', '\r']).trim_end().to_owned();
        self.cursor = 0;
        self.recall = None;
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
            if self.history.len() > MAX_HISTORY {
                self.history.remove(0);
            }
        }
        line
    }

    /// Inserts `text`, which the caller has already cleaned, at the cursor.
    pub fn insert(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.recall = None;
    }

    /// Stashes a written prompt, or brings the newest stashed one back when the
    /// line is empty.
    pub fn stash_toggle(&mut self) -> Stashed {
        if !self.text.trim().is_empty() {
            let text = std::mem::take(&mut self.text);
            self.push_stash(text);
            self.cursor = 0;
            self.recall = None;
            return Stashed::Saved;
        }
        match self.stash.pop() {
            Some(stashed) => {
                self.restore(&stashed);
                Stashed::Restored
            }
            None => Stashed::Nothing,
        }
    }

    /// The word the cursor ends, when it starts with `marker`, with where it starts.
    /// A cursor inside a word ends no word.
    #[must_use]
    pub fn token_at_cursor(&self, marker: char) -> Option<(usize, &str)> {
        if !self.text[self.cursor..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
        {
            return None;
        }
        let before = &self.text[..self.cursor];
        let start = before
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map_or(0, |(at, ch)| at + ch.len_utf8());
        let token = &before[start..];
        token.starts_with(marker).then_some((start, token))
    }

    /// Replaces the text from `start` to the cursor with `with`.
    pub fn replace_to_cursor(&mut self, start: usize, with: &str) {
        self.text.replace_range(start..self.cursor, with);
        self.cursor = start + with.len();
        self.recall = None;
    }

    fn backspace(&mut self) {
        let before = self.previous_boundary();
        if before < self.cursor {
            self.text.replace_range(before..self.cursor, "");
            self.cursor = before;
            self.recall = None;
        }
    }

    fn delete(&mut self) {
        let after = self.next_boundary();
        if after > self.cursor {
            self.text.replace_range(self.cursor..after, "");
            self.recall = None;
        }
    }

    fn delete_word_back(&mut self) {
        let start = self.word_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.recall = None;
    }

    fn delete_to_line_start(&mut self) {
        let start = self.line_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.recall = None;
    }

    fn previous_boundary(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(at, _)| at)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |ch| self.cursor + ch.len_utf8())
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |at| at + 1)
    }

    fn word_start(&self) -> usize {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(|ch: char| !is_word(ch));
        trimmed.trim_end_matches(is_word).len()
    }

    fn word_end(&self) -> usize {
        let after = &self.text[self.cursor..];
        let skipped = after.len() - after.trim_start_matches(|ch: char| !is_word(ch)).len();
        let word = after[skipped..].len() - after[skipped..].trim_start_matches(is_word).len();
        self.cursor + skipped + word
    }

    /// Whether the line still shows the recalled entry, unedited.
    fn showing_recall(&self) -> Option<usize> {
        self.recall
            .filter(|at| self.history.get(*at) == Some(&self.text))
    }

    fn up(&mut self) {
        if self.text.is_empty() && !self.history.is_empty() {
            self.show_history(self.history.len() - 1);
        } else if let Some(at) = self.showing_recall() {
            if at > 0 {
                self.show_history(at - 1);
            }
        } else {
            self.move_line(true);
        }
    }

    fn down(&mut self) {
        if let Some(at) = self.showing_recall() {
            if at + 1 < self.history.len() {
                self.show_history(at + 1);
            } else {
                self.clear();
            }
        } else {
            self.move_line(false);
        }
    }

    fn show_history(&mut self, at: usize) {
        self.text.clone_from(&self.history[at]);
        self.cursor = self.text.len();
        self.recall = Some(at);
    }

    /// The columns a drawn row has for text: one is kept free for the cursor.
    fn text_columns(room: usize) -> usize {
        room.saturating_sub(1).max(1)
    }

    /// The drawn rows at the width of the last draw.
    fn current_rows(&self) -> Vec<Row> {
        self.layout(Self::text_columns(self.room.get()))
    }

    /// Where the drawn row holding the cursor starts and ends.
    fn row_bounds(&self) -> (usize, usize) {
        let rows = self.current_rows();
        let row = rows[self.cursor_row(&rows)];
        (row.start, row.end)
    }

    /// Moves the cursor one drawn row up or down, keeping its column where it can.
    fn move_line(&mut self, up: bool) {
        let rows = self.current_rows();
        let current = self.cursor_row(&rows);
        let target = if up {
            match current.checked_sub(1) {
                Some(row) => rows[row],
                None => return,
            }
        } else {
            match rows.get(current + 1) {
                Some(row) => *row,
                None => return,
            }
        };
        let column = self.text[rows[current].start..self.cursor].width();
        self.cursor = target.start + offset_at_column(&self.text[target.start..target.end], column);
    }

    /// Splits the text into the rows drawn in `room` columns.
    fn layout(&self, room: usize) -> Vec<Row> {
        let room = room.max(1);
        let mut rows = Vec::new();
        let mut line_start = 0;
        for line in self.text.split('\n') {
            let mut row_start = line_start;
            let mut used = 0;
            for (at, ch) in line.char_indices() {
                let width = ch.width().unwrap_or(0);
                if used + width > room && used > 0 {
                    rows.push(Row {
                        start: row_start,
                        end: line_start + at,
                    });
                    row_start = line_start + at;
                    used = 0;
                }
                used += width;
            }
            rows.push(Row {
                start: row_start,
                end: line_start + line.len(),
            });
            line_start += line.len() + 1;
        }
        rows
    }

    /// The index of the row holding the cursor. A cursor at a soft wrap belongs
    /// to the row that starts there.
    fn cursor_row(&self, rows: &[Row]) -> usize {
        rows.iter()
            .enumerate()
            .rev()
            .find(|(_, row)| row.start <= self.cursor)
            .map_or(0, |(at, _)| at)
    }

    /// How many rows the prompt wants in `width` columns, up to [`MAX_ROWS`].
    #[must_use]
    pub fn rows(&self, width: u16) -> u16 {
        let room = usize::from(width).saturating_sub(PROMPT.width());
        self.room.set(room.max(1));
        let wanted = u16::try_from(self.layout(Self::text_columns(room)).len()).unwrap_or(MAX_ROWS);
        wanted.clamp(1, MAX_ROWS)
    }

    /// Draws the prompt, scrolled so the cursor row shows, and puts the
    /// terminal cursor on the cursor when `focused`.
    pub fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let room = usize::from(area.width).saturating_sub(PROMPT.width());
        self.room.set(room.max(1));
        let rows = self.layout(Self::text_columns(room));
        let cursor_row = self.cursor_row(&rows);
        let height = usize::from(area.height).max(1);
        let first = (cursor_row + 1).saturating_sub(height);
        let lines: Vec<String> = rows
            .iter()
            .enumerate()
            .skip(first)
            .take(height)
            .map(|(at, row)| {
                let prefix = if at == 0 { PROMPT } else { CONTINUATION };
                format!("{prefix}{}", &self.text[row.start..row.end])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines.join("\n")), area);
        if focused && area.width > 0 && area.height > 0 {
            let column = self.text[rows[cursor_row].start..self.cursor].width();
            let x = area.x + u16::try_from(PROMPT.width() + column).unwrap_or(u16::MAX);
            let y = area.y + u16::try_from(cursor_row - first).unwrap_or(0);
            frame.set_cursor_position(Position::new(
                x.min(area.x + area.width - 1),
                y.min(area.y + area.height - 1),
            ));
        }
    }
}

fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// The byte offset in `line` of the character that starts at or after `column`,
/// or the end of the line.
fn offset_at_column(line: &str, column: usize) -> usize {
    let mut used = 0;
    for (at, ch) in line.char_indices() {
        if used >= column {
            return at;
        }
        used += ch.width().unwrap_or(0);
    }
    line.len()
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

    fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        key(code, modifiers, KeyEventKind::Press)
    }

    fn type_text(input: &mut Input, text: &str) {
        for ch in text.chars() {
            assert_eq!(input.handle_key(press(KeyCode::Char(ch))), None);
        }
    }

    fn submit(input: &mut Input, text: &str) {
        type_text(input, text);
        assert_eq!(
            input.handle_key(press(KeyCode::Enter)).as_deref(),
            Some(text)
        );
    }

    fn cursor_after_drawing(input: &Input, width: u16, height: u16) -> (u16, u16) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| input.render(frame, frame.area(), true))
            .unwrap();
        let place = terminal.get_cursor_position().unwrap();
        (place.x, place.y)
    }

    fn draw(input: &Input, width: u16, height: u16) -> String {
        screen(width, height, |frame| {
            input.render(frame, frame.area(), true);
        })
    }

    #[test]
    fn typed_characters_build_the_line() {
        // Arrange
        let mut input = Input::new();

        // Act
        type_text(&mut input, "hi q");

        // Assert
        assert_eq!(input.text(), "hi q");
        assert_eq!(draw(&input, 10, 1), "> hi q    ");
    }

    #[test]
    fn backspace_removes_the_character_before_the_cursor_and_is_safe_when_empty() {
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
    fn releases_and_unbound_chords_are_ignored() {
        // Arrange
        let mut input = Input::new();

        // Act
        input.handle_key(key(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        input.handle_key(with(KeyCode::Char('z'), KeyModifiers::CONTROL));
        input.handle_key(with(KeyCode::Char('z'), KeyModifiers::ALT));

        // Assert
        assert_eq!(input.text(), "");
    }

    #[test]
    fn the_newline_keys_add_a_line_and_never_submit() {
        for newline in [
            with(KeyCode::Enter, KeyModifiers::ALT),
            with(KeyCode::Enter, KeyModifiers::SHIFT),
            with(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ] {
            // Arrange
            let mut input = Input::new();
            type_text(&mut input, "one");

            // Act
            let submitted = input.handle_key(newline);
            type_text(&mut input, "two");

            // Assert
            assert_eq!(submitted, None);
            assert_eq!(input.text(), "one\ntwo", "{newline:?}");
        }
    }

    #[test]
    fn the_cursor_moves_by_character_and_edits_happen_at_it() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "a\u{4e2d}c");

        // Act
        input.handle_key(press(KeyCode::Left));
        input.handle_key(press(KeyCode::Left));
        type_text(&mut input, "X");
        input.handle_key(press(KeyCode::Right));
        input.handle_key(press(KeyCode::Delete));
        type_text(&mut input, "Y");

        // Assert
        assert_eq!(input.text(), "aX\u{4e2d}Y");
    }

    #[test]
    fn home_end_and_the_chords_move_to_the_ends_of_the_line() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "abc");
        input.handle_key(with(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut input, "def");

        // Act
        input.handle_key(press(KeyCode::Home));
        type_text(&mut input, "1");
        input.handle_key(press(KeyCode::End));
        type_text(&mut input, "2");
        input.handle_key(with(KeyCode::Char('a'), KeyModifiers::CONTROL));
        type_text(&mut input, "3");
        input.handle_key(with(KeyCode::Char('b'), KeyModifiers::CONTROL));
        type_text(&mut input, "4");
        input.handle_key(with(KeyCode::Char('f'), KeyModifiers::CONTROL));
        input.handle_key(with(KeyCode::Char('f'), KeyModifiers::CONTROL));
        type_text(&mut input, "5");

        // Assert
        assert_eq!(input.text(), "abc\n4315def2");
    }

    #[test]
    fn words_are_skipped_and_deleted_whole() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "one two  three");

        // Act
        input.handle_key(with(KeyCode::Left, KeyModifiers::CONTROL));
        input.handle_key(with(KeyCode::Char('b'), KeyModifiers::ALT));
        input.handle_key(with(KeyCode::Char('f'), KeyModifiers::ALT));
        input.handle_key(with(KeyCode::Char('w'), KeyModifiers::CONTROL));

        // Assert
        assert_eq!(input.text(), "one   three");
    }

    #[test]
    fn control_u_deletes_back_to_the_start_of_the_line() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "keep");
        input.handle_key(with(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut input, "drop this");

        // Act
        input.handle_key(with(KeyCode::Char('u'), KeyModifiers::CONTROL));

        // Assert
        assert_eq!(input.text(), "keep\n");
    }

    #[test]
    fn up_and_down_move_between_lines_keeping_the_column() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "abcdef");
        input.handle_key(with(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut input, "xy");
        input.handle_key(with(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut input, "123456");

        // Act
        input.handle_key(press(KeyCode::Up));
        type_text(&mut input, "|");
        input.handle_key(press(KeyCode::Up));
        type_text(&mut input, "#");
        input.handle_key(press(KeyCode::Down));
        input.handle_key(press(KeyCode::Down));
        type_text(&mut input, "$");

        // Assert
        assert_eq!(input.text(), "abc#def\nxy|\n123$456");
    }

    #[test]
    fn up_recalls_earlier_prompts_only_on_an_empty_or_unedited_line() {
        // Arrange
        let mut input = Input::new();
        submit(&mut input, "first");
        submit(&mut input, "second");

        // Act
        input.handle_key(press(KeyCode::Up));
        let newest = input.text().to_owned();
        input.handle_key(press(KeyCode::Up));
        let older = input.text().to_owned();
        input.handle_key(press(KeyCode::Up));
        let oldest_again = input.text().to_owned();
        input.handle_key(press(KeyCode::Down));
        let back = input.text().to_owned();
        input.handle_key(press(KeyCode::Down));
        let empty = input.text().to_owned();

        // Assert
        assert_eq!(
            (newest.as_str(), older.as_str(), oldest_again.as_str()),
            ("second", "first", "first")
        );
        assert_eq!((back.as_str(), empty.as_str()), ("second", ""));
    }

    #[test]
    fn up_does_not_recall_over_text_the_user_wrote_or_edited() {
        // Arrange
        let mut input = Input::new();
        submit(&mut input, "earlier");
        type_text(&mut input, "draft");

        // Act
        input.handle_key(press(KeyCode::Up));
        let kept_draft = input.text().to_owned();
        input.clear();
        input.handle_key(press(KeyCode::Up));
        type_text(&mut input, "!");
        input.handle_key(press(KeyCode::Up));

        // Assert
        assert_eq!(kept_draft, "draft");
        assert_eq!(input.text(), "earlier!");
    }

    #[test]
    fn a_repeated_prompt_is_remembered_once() {
        // Arrange
        let mut input = Input::new();
        submit(&mut input, "same");
        submit(&mut input, "same");

        // Act
        input.handle_key(press(KeyCode::Up));
        input.handle_key(press(KeyCode::Up));

        // Assert
        assert_eq!(input.text(), "same");
        assert_eq!(input.history.len(), 1);
    }

    #[test]
    fn recall_remembers_a_limited_number_of_prompts() {
        // Arrange
        let mut input = Input::new();
        for number in 0..=MAX_HISTORY {
            submit(&mut input, &number.to_string());
        }

        // Act / Assert
        assert_eq!(input.history.len(), MAX_HISTORY);
        assert_eq!(input.history[0], "1");
    }

    #[test]
    fn the_stash_keeps_a_prompt_and_gives_the_newest_back_on_an_empty_line() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "first draft");
        let saved_first = input.stash_toggle();
        type_text(&mut input, "second draft");
        let saved_second = input.stash_toggle();

        // Act
        let empty_line = input.text().to_owned();
        let restored = input.stash_toggle();
        let newest = input.text().to_owned();
        input.clear();
        let restored_again = input.stash_toggle();
        let oldest = input.text().to_owned();
        input.clear();
        let nothing = input.stash_toggle();

        // Assert
        assert_eq!(
            (saved_first, saved_second),
            (Stashed::Saved, Stashed::Saved)
        );
        assert_eq!(empty_line, "");
        assert_eq!(
            (restored, newest.as_str()),
            (Stashed::Restored, "second draft")
        );
        assert_eq!(
            (restored_again, oldest.as_str()),
            (Stashed::Restored, "first draft")
        );
        assert_eq!(nothing, Stashed::Nothing);
    }

    #[test]
    fn inserted_text_goes_in_at_the_cursor_and_the_cursor_follows_it() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "ab");
        input.handle_key(press(KeyCode::Left));

        // Act
        input.insert("x\ny");
        type_text(&mut input, "!");

        // Assert
        assert_eq!(input.text(), "ax\ny!b");
    }

    #[test]
    fn a_cursor_inside_a_word_has_no_token_to_complete() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "@src/main.rs");
        for _ in 0..4 {
            input.handle_key(press(KeyCode::Left));
        }

        // Act / Assert
        assert_eq!(input.token_at_cursor('@'), None);
        input.handle_key(press(KeyCode::End));
        assert!(input.token_at_cursor('@').is_some());
        type_text(&mut input, " next");
        for _ in 0..5 {
            input.handle_key(press(KeyCode::Left));
        }
        assert_eq!(input.token_at_cursor('@'), Some((0, "@src/main.rs")));
    }

    #[test]
    fn the_token_at_the_cursor_starts_at_the_marker_after_a_space_or_newline() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "read\n@src/ma");

        // Act
        let token = input
            .token_at_cursor('@')
            .map(|(at, token)| (at, token.to_owned()));
        input.replace_to_cursor(5, "@src/main.rs ");

        // Assert
        assert_eq!(token, Some((5, "@src/ma".to_owned())));
        assert_eq!(input.text(), "read\n@src/main.rs ");
        assert_eq!(input.token_at_cursor('@'), None);
    }

    #[test]
    fn a_word_without_the_marker_is_not_a_token_and_a_wide_space_ends_one() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "mail me@home");
        let mut wide = Input::new();
        type_text(&mut wide, "x\u{3000}@ab");

        // Act / Assert
        assert_eq!(input.token_at_cursor('@'), None);
        assert_eq!(wide.token_at_cursor('@'), Some((4, "@ab")));
    }

    #[test]
    fn a_long_line_wraps_and_the_prompt_asks_for_the_rows_it_needs() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "abcdefghij");

        // Act
        let rows = input.rows(7);
        let shown = draw(&input, 7, 3);

        // Assert
        assert_eq!(rows, 3);
        assert_eq!(shown, "> abcd \n  efgh \n  ij   ");
    }

    #[test]
    fn a_full_row_leaves_a_column_for_the_cursor_after_the_last_letter() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "abcd");

        // Act
        let place = cursor_after_drawing(&input, 7, 2);

        // Assert
        assert_eq!(place, (6, 0));
    }

    #[test]
    fn the_cursor_is_on_the_row_after_a_trailing_newline_and_at_a_soft_wrap() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "ab");
        input.handle_key(with(KeyCode::Enter, KeyModifiers::ALT));
        let after_newline = cursor_after_drawing(&input, 10, 3);
        let mut wrapped = Input::new();
        type_text(&mut wrapped, "abcde");

        // Act
        let at_wrap = cursor_after_drawing(&wrapped, 7, 3);

        // Assert
        assert_eq!(after_newline, (2, 1));
        assert_eq!(at_wrap, (3, 1));
    }

    #[test]
    fn up_down_home_and_end_follow_the_drawn_rows_of_a_wrapped_line() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "abcdefghij");
        draw(&input, 7, 3);

        // Act
        input.handle_key(press(KeyCode::Up));
        type_text(&mut input, "1");
        input.handle_key(press(KeyCode::Home));
        type_text(&mut input, "2");
        input.handle_key(press(KeyCode::End));
        type_text(&mut input, "3");
        input.handle_key(press(KeyCode::Down));
        type_text(&mut input, "4");

        // Assert
        assert_eq!(input.text(), "abcd2ef13ghij4");
    }

    #[test]
    fn moving_up_keeps_the_column_across_wide_characters() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "\u{4e2d}\u{6587}\u{4e2d}");
        input.handle_key(with(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut input, "abcd");

        // Act
        input.handle_key(press(KeyCode::Up));
        type_text(&mut input, "|");

        // Assert
        assert_eq!(input.text(), "\u{4e2d}\u{6587}|\u{4e2d}\nabcd");
    }

    #[test]
    fn alt_backspace_deletes_a_word_and_delete_at_the_end_does_nothing() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "one two");

        // Act
        input.handle_key(press(KeyCode::Delete));
        input.handle_key(with(KeyCode::Backspace, KeyModifiers::ALT));

        // Assert
        assert_eq!(input.text(), "one ");
    }

    #[test]
    fn a_submitted_line_loses_trailing_blanks_and_leading_empty_lines() {
        // Arrange
        let mut input = Input::new();
        input.handle_key(with(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut input, "  keep indent  ");
        input.handle_key(with(KeyCode::Enter, KeyModifiers::ALT));

        // Act
        let submitted = input.handle_key(press(KeyCode::Enter));

        // Assert
        assert_eq!(submitted.as_deref(), Some("  keep indent"));
    }

    #[test]
    fn discarding_a_draft_keeps_it_in_the_stash_and_an_empty_line_stashes_nothing() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "long draft");

        // Act
        let discarded = input.discard();
        let draft_gone = input.text().to_owned();
        let nothing = input.discard();
        let restored = input.stash_toggle();

        // Assert
        assert!(discarded);
        assert_eq!(draft_gone, "");
        assert!(!nothing);
        assert_eq!((restored, input.text()), (Stashed::Restored, "long draft"));
    }

    #[test]
    fn the_stash_keeps_a_limited_number_of_prompts() {
        // Arrange
        let mut input = Input::new();
        for number in 0..=MAX_HISTORY {
            type_text(&mut input, &number.to_string());
            input.stash_toggle();
        }

        // Act / Assert
        assert_eq!(input.stash.len(), MAX_HISTORY);
        assert_eq!(input.stash[0], "1");
    }

    #[test]
    fn the_prompt_never_asks_for_more_than_its_maximum_rows_and_scrolls_to_the_cursor() {
        // Arrange
        let mut input = Input::new();
        for number in 0..10 {
            type_text(&mut input, &number.to_string());
            input.handle_key(with(KeyCode::Enter, KeyModifiers::ALT));
        }
        type_text(&mut input, "end");

        // Act
        let rows = input.rows(20);
        let shown = draw(&input, 20, MAX_ROWS);

        // Assert
        assert_eq!(rows, MAX_ROWS);
        assert!(
            shown.lines().last().unwrap().starts_with("  end"),
            "{shown}"
        );
        assert!(!shown.contains("> 0"), "{shown}");
    }

    #[test]
    fn the_cursor_is_drawn_where_the_text_is_edited_and_only_when_focused() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "ab\u{4e2d}");
        input.handle_key(press(KeyCode::Left));
        let place = |focused: bool| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(10, 1)).unwrap();
            terminal
                .draw(|frame| input.render(frame, frame.area(), focused))
                .unwrap();
            terminal.get_cursor_position().unwrap()
        };

        // Act
        let focused = place(true);

        // Assert
        assert_eq!((focused.x, focused.y), (4, 0));
        assert_ne!((place(false).x, place(false).y), (4, 0));
    }

    #[test]
    fn wide_characters_and_tiny_areas_do_not_panic() {
        // Arrange
        let mut input = Input::new();
        type_text(&mut input, "\u{4e2d}\u{6587}\u{4e2d}\u{6587}");

        // Act / Assert
        for width in [0, 1, 2, 3, 7] {
            for height in [0, 1, 3] {
                let _ = screen(width, height, |frame| {
                    input.render(frame, frame.area(), true);
                });
            }
        }
        let _ = input.rows(0);
    }

    #[test]
    fn restore_puts_text_back_with_the_cursor_at_its_end() {
        // Arrange
        let mut input = Input::new();

        // Act
        input.restore("one\ntwo");
        type_text(&mut input, "!");

        // Assert
        assert_eq!(input.text(), "one\ntwo!");
    }
}
