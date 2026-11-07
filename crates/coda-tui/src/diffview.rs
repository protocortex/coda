// SPDX-License-Identifier: Apache-2.0

//! The `/diff` overlay: what the last turn changed, as a colored unified diff
//! that scrolls and can be searched. It is drawn over the transcript and takes
//! the keys while it is open.

use std::cell::Cell;

use coda_protocol::methods::SessionDiffResult;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::permission::sanitize;

const TAB_STOP: &str = "    ";
const CUT_NOTE: &str = "(the diff was cut because it is too large to show in full)";
const NO_PATCH_NOTE: &str =
    "(no diff text: the change is too large to show; the files are listed above in the title)";
const SEARCH_PROMPT: &str = "/";
const NO_MATCH: &str = "no match";
const PAGE_FALLBACK_ROWS: usize = 20;
const SIDE_STEP: usize = 8;
const HEAD_PREFIXES: [&str; 6] = [
    "diff --git",
    "index ",
    "--- ",
    "+++ ",
    "new file",
    "deleted file",
];

/// What a key did to the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Close,
}

/// A diff being read.
#[derive(Debug)]
pub struct DiffView {
    title: String,
    lines: Vec<String>,
    top: usize,
    left: usize,
    query: String,
    searching: bool,
    matches: Vec<usize>,
    /// Which of `matches` the view is on.
    current: usize,
    /// Rows of text the last draw had room for.
    rows: Cell<usize>,
}

impl DiffView {
    #[must_use]
    pub fn new(result: &SessionDiffResult) -> Self {
        let added: u64 = result.files.iter().map(|file| file.added).sum();
        let deleted: u64 = result.files.iter().map(|file| file.deleted).sum();
        let count = result.files.len();
        let title = format!(
            " Turn changes: {count} {} +{added} -{deleted} (Esc closes, / searches, n and N jump) ",
            if count == 1 { "file" } else { "files" }
        );
        let mut lines: Vec<String> = result
            .patch
            .lines()
            .map(|line| sanitize(&line.replace('\t', TAB_STOP)).0)
            .collect();
        if result.patch.is_empty() {
            lines.push(NO_PATCH_NOTE.to_owned());
        }
        if result.truncated && !result.patch.is_empty() {
            lines.push(CUT_NOTE.to_owned());
        }
        Self {
            title,
            lines,
            top: 0,
            left: 0,
            query: String::new(),
            searching: false,
            matches: Vec::new(),
            current: 0,
            rows: Cell::new(PAGE_FALLBACK_ROWS),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        if self.searching {
            self.search_key(key);
            return Action::None;
        }
        let page = self.rows.get().max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Action::Close,
            KeyCode::Char('/') => {
                self.searching = true;
                self.query.clear();
                self.matches.clear();
            }
            KeyCode::Char('n') => self.jump(true),
            KeyCode::Char('N') => self.jump(false),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_down(1),
            KeyCode::Up | KeyCode::Char('k') => self.top = self.top.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_down(page),
            KeyCode::PageUp | KeyCode::Char('b') => self.top = self.top.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => self.top = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll_down(self.lines.len()),
            KeyCode::Right | KeyCode::Char('l') => {
                let widest = self.lines.iter().map(|line| line.chars().count()).max();
                self.left = (self.left + SIDE_STEP).min(widest.unwrap_or(0).saturating_sub(1));
            }
            KeyCode::Left | KeyCode::Char('h') => self.left = self.left.saturating_sub(SIDE_STEP),
            _ => {}
        }
        Action::None
    }

    fn search_key(&mut self, key: KeyEvent) {
        let plain = !key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.searching = false;
                self.query.clear();
                self.matches.clear();
            }
            KeyCode::Enter => self.searching = false,
            KeyCode::Backspace => {
                self.query.pop();
                self.find();
            }
            KeyCode::Char(ch) if plain => {
                self.query.push(ch);
                self.find();
            }
            _ => {}
        }
    }

    /// Finds the lines holding the query, case blind, and goes to the first
    /// one at or below the top of the view.
    fn find(&mut self) {
        self.matches.clear();
        self.current = 0;
        if self.query.is_empty() {
            return;
        }
        let wanted = self.query.to_lowercase();
        self.matches = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.to_lowercase().contains(&wanted))
            .map(|(at, _)| at)
            .collect();
        self.current = self
            .matches
            .iter()
            .position(|&at| at >= self.top)
            .unwrap_or(0);
        self.show_current();
    }

    fn jump(&mut self, forward: bool) {
        if self.matches.is_empty() {
            return;
        }
        let count = self.matches.len();
        self.current = if forward {
            (self.current + 1) % count
        } else {
            (self.current + count - 1) % count
        };
        self.show_current();
    }

    fn show_current(&mut self) {
        if let Some(&line) = self.matches.get(self.current) {
            let rows = self.rows.get().max(1);
            if line < self.top || line >= self.top + rows {
                self.top = line.saturating_sub(rows / 2);
            }
        }
    }

    fn scroll_down(&mut self, by: usize) {
        let last = self.lines.len().saturating_sub(self.rows.get().max(1));
        self.top = self.top.saturating_add(by).min(last);
    }

    fn status(&self) -> String {
        if self.query.is_empty() {
            return if self.searching {
                SEARCH_PROMPT.to_owned()
            } else {
                String::new()
            };
        }
        if self.matches.is_empty() {
            return format!("{SEARCH_PROMPT}{}  {NO_MATCH}", self.query);
        }
        format!(
            "{SEARCH_PROMPT}{}  match {} of {}",
            self.query,
            self.current + 1,
            self.matches.len()
        )
    }

    fn style_of(&self, at: usize, line: &str) -> Style {
        let base = if HEAD_PREFIXES.iter().any(|head| line.starts_with(head)) {
            Style::default().add_modifier(Modifier::BOLD)
        } else if line.starts_with("@@") {
            Style::default().fg(Color::Cyan)
        } else if line.starts_with('+') {
            Style::default().fg(Color::Green)
        } else if line.starts_with('-') {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        match self.matches.iter().position(|&hit| hit == at) {
            Some(hit) if hit == self.current => base.add_modifier(Modifier::REVERSED),
            Some(_) => base.add_modifier(Modifier::UNDERLINED),
            None => base,
        }
    }

    pub fn render(&self, frame: &mut Frame, screen: Rect) {
        if screen.width == 0 || screen.height == 0 {
            return;
        }
        frame.render_widget(Clear, screen);
        let block = Block::bordered().title(self.title.as_str());
        let inner = block.inner(screen);
        frame.render_widget(block, screen);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let rows = usize::from(inner.height.saturating_sub(1)).max(1);
        self.rows.set(rows);
        let lines: Vec<Line> = self
            .lines
            .iter()
            .enumerate()
            .skip(self.top)
            .take(rows)
            .map(|(at, line)| Line::from(Span::styled(line.clone(), self.style_of(at, line))))
            .collect();
        let text = Rect {
            height: inner.height.saturating_sub(1),
            ..inner
        };
        let side = u16::try_from(self.left).unwrap_or(u16::MAX);
        frame.render_widget(Paragraph::new(lines).scroll((0, side)), text);
        let status = Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        };
        frame.render_widget(Paragraph::new(self.status()), status);
    }
}

#[cfg(test)]
mod tests {
    use coda_protocol::methods::DiffFile;
    use coda_protocol::types::TurnId;
    use crossterm::event::KeyEventState;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;
    use crate::test_support::screen;

    const PATCH: &str = "diff --git a/a.txt b/a.txt\nindex 1..2 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n";

    fn result(patch: &str, truncated: bool) -> SessionDiffResult {
        SessionDiffResult {
            turn_id: TurnId::new("1"),
            files: vec![
                DiffFile {
                    path: "a.txt".to_owned(),
                    added: 1,
                    deleted: 1,
                    binary: false,
                },
                DiffFile {
                    path: "b.txt".to_owned(),
                    added: 4,
                    deleted: 0,
                    binary: false,
                },
            ],
            patch: patch.to_owned(),
            truncated,
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn type_text(view: &mut DiffView, text: &str) {
        for ch in text.chars() {
            view.handle_key(press(KeyCode::Char(ch)));
        }
    }

    fn drawn(view: &DiffView, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn first_row(shown: &str) -> &str {
        shown.lines().nth(1).unwrap_or_default()
    }

    fn long_patch(lines: usize) -> String {
        (0..lines)
            .flat_map(|n| [format!("+line {n}"), "\n".to_owned()])
            .collect()
    }

    #[test]
    fn the_title_counts_the_files_and_the_lines() {
        // Arrange
        let view = DiffView::new(&result(PATCH, false));

        // Act
        let shown = screen(100, 12, |frame| view.render(frame, frame.area()));

        // Assert
        assert!(shown.contains("Turn changes: 2 files +5 -1"), "{shown}");
        assert!(shown.contains("-one"), "{shown}");
        assert!(shown.contains("+two"), "{shown}");
    }

    #[test]
    fn added_and_removed_lines_and_hunk_headers_are_colored() {
        // Arrange
        let view = DiffView::new(&result(PATCH, false));

        // Act
        let buffer = drawn(&view, 100, 12);

        // Assert
        let color_of = |row: u16| buffer[(1, row)].fg;
        assert_eq!(color_of(5), Color::Cyan, "hunk header");
        assert_eq!(color_of(6), Color::Red, "removed line");
        assert_eq!(color_of(7), Color::Green, "added line");
        assert_eq!(
            color_of(1),
            Color::Reset,
            "file header is bold, not colored"
        );
    }

    #[test]
    fn escape_and_q_close_the_view() {
        // Arrange
        let mut view = DiffView::new(&result(PATCH, false));

        // Act
        let escape = view.handle_key(press(KeyCode::Esc));
        let q = view.handle_key(press(KeyCode::Char('q')));

        // Assert
        assert_eq!((escape, q), (Action::Close, Action::Close));
    }

    #[test]
    fn the_arrow_keys_and_pages_scroll_and_stop_at_the_ends() {
        // Arrange
        let mut view = DiffView::new(&result(&long_patch(100), false));
        let _ = drawn(&view, 60, 13);

        // Act
        view.handle_key(press(KeyCode::Down));
        let one = screen(60, 13, |frame| view.render(frame, frame.area()));
        view.handle_key(press(KeyCode::PageDown));
        let page = screen(60, 13, |frame| view.render(frame, frame.area()));
        view.handle_key(press(KeyCode::End));
        let end = screen(60, 13, |frame| view.render(frame, frame.area()));
        view.handle_key(press(KeyCode::End));
        view.handle_key(press(KeyCode::Home));
        let home = screen(60, 13, |frame| view.render(frame, frame.area()));

        // Assert
        assert!(first_row(&one).starts_with("│+line 1 "), "{one}");
        assert!(first_row(&page).starts_with("│+line 11 "), "{page}");
        assert!(end.contains("+line 99 "), "{end}");
        assert!(first_row(&home).starts_with("│+line 0 "), "{home}");
    }

    #[test]
    fn searching_finds_lines_case_blind_and_jumps_between_matches() {
        // Arrange
        let mut view = DiffView::new(&result(&long_patch(100), false));
        let _ = drawn(&view, 60, 13);

        // Act
        view.handle_key(press(KeyCode::Char('/')));
        type_text(&mut view, "LINE 5");
        let typing = screen(60, 13, |frame| view.render(frame, frame.area()));
        view.handle_key(press(KeyCode::Enter));
        let first = screen(60, 13, |frame| view.render(frame, frame.area()));
        view.handle_key(press(KeyCode::Char('n')));
        let second = screen(60, 13, |frame| view.render(frame, frame.area()));
        view.handle_key(press(KeyCode::Char('N')));
        view.handle_key(press(KeyCode::Char('N')));
        let wrapped = screen(60, 13, |frame| view.render(frame, frame.area()));

        // Assert
        assert!(typing.contains("/LINE 5"), "{typing}");
        assert!(first.contains("match 1 of 11"), "{first}");
        assert!(first.contains("+line 5 "), "{first}");
        assert!(second.contains("match 2 of 11"), "{second}");
        assert!(wrapped.contains("match 11 of 11"), "{wrapped}");
    }

    #[test]
    fn a_search_with_no_hit_says_so_and_escape_leaves_the_search_not_the_view() {
        // Arrange
        let mut view = DiffView::new(&result(PATCH, false));

        // Act
        view.handle_key(press(KeyCode::Char('/')));
        type_text(&mut view, "zzz");
        let shown = screen(100, 12, |frame| view.render(frame, frame.area()));
        let leave = view.handle_key(press(KeyCode::Esc));
        let after = screen(100, 12, |frame| view.render(frame, frame.area()));

        // Assert
        assert!(shown.contains("no match"), "{shown}");
        assert_eq!(leave, Action::None);
        assert!(!after.contains("no match"), "{after}");
        assert_eq!(view.handle_key(press(KeyCode::Esc)), Action::Close);
    }

    #[test]
    fn typing_while_searching_does_not_act_as_a_command() {
        // Arrange
        let mut view = DiffView::new(&result(PATCH, false));

        // Act
        view.handle_key(press(KeyCode::Char('/')));
        let action = view.handle_key(press(KeyCode::Char('q')));

        // Assert
        assert_eq!(action, Action::None);
    }

    #[test]
    fn the_current_match_is_reversed_and_the_others_underlined() {
        // Arrange
        let mut view = DiffView::new(&result("+one\n+two\n+one more\n", false));
        let _ = drawn(&view, 60, 10);
        view.handle_key(press(KeyCode::Char('/')));
        type_text(&mut view, "one");

        // Act
        let buffer = drawn(&view, 60, 10);

        // Assert
        assert!(buffer[(1, 1)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(1, 2)].modifier.contains(Modifier::UNDERLINED));
        assert!(buffer[(1, 3)].modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn control_characters_in_the_diff_are_drawn_safely() {
        // Arrange
        let view = DiffView::new(&result("+ok\u{1b}[31mred\u{7}\n+tab\there\n", false));

        // Act
        let shown = screen(60, 8, |frame| view.render(frame, frame.area()));

        // Assert
        assert!(!shown.contains('\u{1b}'), "{shown:?}");
        assert!(!shown.contains('\u{7}'), "{shown:?}");
        assert!(shown.contains("+tab    here"), "{shown}");
    }

    #[test]
    fn a_cut_diff_and_a_missing_diff_each_carry_a_note() {
        // Arrange
        let cut = DiffView::new(&result(PATCH, true));
        let none = DiffView::new(&result("", true));

        // Act
        let cut_shown = screen(100, 14, |frame| cut.render(frame, frame.area()));
        let none_shown = screen(120, 6, |frame| none.render(frame, frame.area()));

        // Assert
        assert!(
            cut_shown.contains("cut because it is too large"),
            "{cut_shown}"
        );
        assert!(none_shown.contains("no diff text"), "{none_shown}");
    }

    #[test]
    fn right_and_left_scroll_a_long_line_sideways() {
        // Arrange
        let long = format!("+{}END\n", "x".repeat(80));
        let mut view = DiffView::new(&result(&long, false));

        // Act
        let before = screen(40, 6, |frame| view.render(frame, frame.area()));
        for _ in 0..10 {
            view.handle_key(press(KeyCode::Right));
        }
        let moved = screen(40, 6, |frame| view.render(frame, frame.area()));
        for _ in 0..20 {
            view.handle_key(press(KeyCode::Left));
        }
        let back = screen(40, 6, |frame| view.render(frame, frame.area()));

        // Assert
        assert!(!before.contains("END"), "{before}");
        assert!(moved.contains("END"), "{moved}");
        assert_eq!(before, back);
    }

    #[test]
    fn scrolling_right_stops_at_the_end_of_the_longest_line() {
        // Arrange
        let mut view = DiffView::new(&result("+short\n", false));

        // Act
        for _ in 0..50 {
            view.handle_key(press(KeyCode::Right));
        }
        let shown = screen(40, 6, |frame| view.render(frame, frame.area()));

        // Assert
        assert!(shown.contains('t'), "{shown}");
        view.handle_key(press(KeyCode::Left));
        let back = screen(40, 6, |frame| view.render(frame, frame.area()));
        assert!(back.contains("short"), "{back}");
    }

    #[test]
    fn a_screen_too_small_to_hold_the_view_does_not_panic() {
        // Arrange
        let view = DiffView::new(&result(PATCH, true));

        // Act and Assert
        for (width, height) in [(0, 0), (1, 1), (2, 2), (3, 3), (10, 2)] {
            let _ = screen(width, height, |frame| view.render(frame, frame.area()));
        }
    }
}
