// SPDX-License-Identifier: Apache-2.0

//! The session picker: an overlay that lists the daemon's sessions, the ones
//! that wait for the user first, narrows them as the user types and resumes the
//! chosen one.

use std::cmp::Reverse;
use std::time::{SystemTime, UNIX_EPOCH};

use unicode_width::UnicodeWidthChar;

use coda_protocol::methods::{SessionAttention, SessionStatus, SessionSummary};
use coda_protocol::types::SessionId;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::permission::sanitize_line;
use crate::status::backend_name;

const TITLE: &str = " Sessions (Esc closes, Enter resumes) ";
const FILTER_LABEL: &str = "filter: ";
const NO_MATCH: &str = "no session matches";
const NO_MESSAGE: &str = "(no message yet)";
const APPROVAL: &str = "approval needed";
const INPUT: &str = "input needed";
const WORKING: &str = "working";
const IDLE: &str = "idle";
/// Width of the column that names what a session waits for.
const STATE_WIDTH: usize = 15;
/// Terminal cells a title may take, so the backend and age stay in view.
const TITLE_CELLS: usize = 40;
const CLIP_MARK: &str = "...";
const POPUP_WIDTH: u16 = 100;
/// Rows the popup keeps for its border and the filter line.
const POPUP_CHROME: u16 = 3;
const PAGE_ROWS: usize = 5;
const MS_PER_SECOND: i64 = 1000;
const SECONDS_PER_MINUTE: i64 = 60;
const MINUTES_PER_HOUR: i64 = 60;
const HOURS_PER_DAY: i64 = 24;

/// What a key did to the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The key only moved or narrowed the list.
    None,
    /// The user closed the picker.
    Close,
    /// The user chose this session.
    Resume(SessionId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    id: SessionId,
    state: &'static str,
    /// Lower sorts first.
    rank: u8,
    updated_at: i64,
    title: String,
    backend: &'static str,
    age: String,
    /// Lower case text the filter looks in.
    haystack: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    rows: Vec<Row>,
    filter: String,
    /// Index into the rows the filter keeps.
    selected: usize,
}

/// Milliseconds since the Unix epoch now.
#[must_use]
pub fn now_ms() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// How long ago `then_ms` was at `now_ms`, in the largest unit that fits.
fn age_text(now_ms: i64, then_ms: i64) -> String {
    let seconds = now_ms.saturating_sub(then_ms).max(0) / MS_PER_SECOND;
    let minutes = seconds / SECONDS_PER_MINUTE;
    let hours = minutes / MINUTES_PER_HOUR;
    let days = hours / HOURS_PER_DAY;
    if days > 0 {
        format!("{days}d ago")
    } else if hours > 0 {
        format!("{hours}h ago")
    } else if minutes > 0 {
        format!("{minutes}m ago")
    } else {
        "just now".to_owned()
    }
}

/// `text` cut to at most `cells` terminal cells, ending in a mark when cut.
fn clip(text: &str, cells: usize) -> String {
    let width = |ch: char| ch.width().unwrap_or(0);
    if text.chars().map(width).sum::<usize>() <= cells {
        return text.to_owned();
    }
    let room = cells.saturating_sub(CLIP_MARK.len());
    let mut used = 0;
    let mut kept: String = text
        .chars()
        .take_while(|&ch| {
            used += width(ch);
            used <= room
        })
        .collect();
    kept.push_str(CLIP_MARK);
    kept
}

fn row(session: &SessionSummary, now_ms: i64) -> Row {
    let (state, rank) = match (session.attention, session.status) {
        (SessionAttention::Approval, _) => (APPROVAL, 0),
        (SessionAttention::Input, _) => (INPUT, 1),
        (_, SessionStatus::Running) => (WORKING, 2),
        _ => (IDLE, 3),
    };
    let title = session
        .title
        .as_deref()
        .map_or_else(|| NO_MESSAGE.to_owned(), sanitize_line);
    let backend = backend_name(session.backend);
    let cwd = sanitize_line(&session.cwd);
    let haystack = format!("{state} {title} {backend} {cwd} {}", session.id).to_lowercase();
    Row {
        id: session.id.clone(),
        state,
        rank,
        updated_at: session.updated_at,
        title,
        backend,
        age: age_text(now_ms, session.updated_at),
        haystack,
    }
}

impl Picker {
    /// Lists `sessions` as of `now_ms`: approvals first, then questions, then
    /// running sessions, then the rest, each group newest first.
    #[must_use]
    pub fn new(sessions: &[SessionSummary], now_ms: i64) -> Self {
        let mut rows: Vec<Row> = sessions.iter().map(|s| row(s, now_ms)).collect();
        rows.sort_by_key(|r| (r.rank, Reverse(r.updated_at)));
        Self {
            rows,
            filter: String::new(),
            selected: 0,
        }
    }

    /// The rows whose text holds every word typed.
    fn visible(&self) -> Vec<&Row> {
        let wanted = self.filter.to_lowercase();
        let words: Vec<&str> = wanted.split_whitespace().collect();
        self.rows
            .iter()
            .filter(|r| words.iter().all(|word| r.haystack.contains(word)))
            .collect()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let count = self.visible().len();
        match key.code {
            KeyCode::Esc => return Action::Close,
            KeyCode::Enter => {
                if let Some(chosen) = self.visible().get(self.selected) {
                    return Action::Resume(chosen.id.clone());
                }
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('p') if ctrl => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(count.saturating_sub(1)),
            KeyCode::Char('n') if ctrl => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(PAGE_ROWS),
            KeyCode::PageDown => {
                self.selected = (self.selected + PAGE_ROWS).min(count.saturating_sub(1));
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char(ch) if !ctrl && !alt => {
                self.filter.push(ch);
                self.selected = 0;
            }
            _ => {}
        }
        Action::None
    }

    fn line(row: &Row) -> String {
        format!(
            "{:<STATE_WIDTH$} {} #{} | {} | {}",
            row.state,
            clip(&row.title, TITLE_CELLS),
            row.id,
            row.backend,
            row.age
        )
    }

    pub fn render(&self, frame: &mut Frame, screen: Rect) {
        if screen.width == 0 || screen.height == 0 {
            return;
        }
        let visible = self.visible();
        let width = POPUP_WIDTH.min(screen.width);
        let wanted = u16::try_from(visible.len().max(1)).unwrap_or(u16::MAX);
        let height = wanted
            .saturating_add(POPUP_CHROME)
            .min(screen.height.saturating_sub(2).max(1));
        let area = Rect {
            x: screen.x + (screen.width - width) / 2,
            y: screen.y + (screen.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        let room = usize::from(height.saturating_sub(POPUP_CHROME)).max(1);
        let first = (self.selected + 1).saturating_sub(room);
        let mut lines = vec![Line::from(format!("{FILTER_LABEL}{}", self.filter))];
        if visible.is_empty() {
            lines.push(Line::from(NO_MATCH));
        }
        for (at, row) in visible.iter().enumerate().skip(first).take(room) {
            let style = if at == self.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(Self::line(row), style)));
        }
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(TITLE)),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use coda_protocol::types::BackendKind;
    use crossterm::event::KeyEventState;

    use super::*;
    use crate::test_support::screen;

    const NOW: i64 = 10_000_000_000;

    fn summary(
        id: &str,
        title: Option<&str>,
        status: SessionStatus,
        attention: SessionAttention,
        ago_ms: i64,
    ) -> SessionSummary {
        SessionSummary {
            id: SessionId::new(id),
            backend: BackendKind::Faux,
            cwd: "/work/repo".to_owned(),
            created_at: NOW - ago_ms,
            updated_at: NOW - ago_ms,
            status,
            attention,
            title: title.map(str::to_owned),
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::NONE)
    }

    fn type_text(picker: &mut Picker, text: &str) {
        for ch in text.chars() {
            assert_eq!(picker.handle_key(press(KeyCode::Char(ch))), Action::None);
        }
    }

    fn ids(picker: &Picker) -> Vec<String> {
        picker
            .visible()
            .iter()
            .map(|r| r.id.as_str().to_owned())
            .collect()
    }

    fn mixed() -> Picker {
        Picker::new(
            &[
                summary(
                    "1",
                    Some("old done"),
                    SessionStatus::Idle,
                    SessionAttention::None,
                    900_000,
                ),
                summary(
                    "2",
                    Some("running now"),
                    SessionStatus::Running,
                    SessionAttention::None,
                    5_000,
                ),
                summary(
                    "3",
                    Some("asked a question"),
                    SessionStatus::Running,
                    SessionAttention::Input,
                    60_000,
                ),
                summary(
                    "4",
                    Some("needs a yes"),
                    SessionStatus::Running,
                    SessionAttention::Approval,
                    120_000,
                ),
                summary(
                    "5",
                    None,
                    SessionStatus::Idle,
                    SessionAttention::None,
                    10_000,
                ),
                summary(
                    "6",
                    Some("older approval"),
                    SessionStatus::Idle,
                    SessionAttention::Approval,
                    300_000,
                ),
            ],
            NOW,
        )
    }

    #[test]
    fn sessions_are_ordered_by_what_they_wait_for_then_newest_first() {
        // Arrange / Act
        let picker = mixed();

        // Assert
        assert_eq!(ids(&picker), ["4", "6", "3", "2", "5", "1"]);
    }

    #[test]
    fn a_long_title_is_cut_so_the_backend_and_age_stay_in_view() {
        // Arrange
        let long = "w".repeat(80);
        let wide = "\u{4e2d}".repeat(40);
        let picker = Picker::new(
            &[
                summary(
                    "1",
                    Some(&long),
                    SessionStatus::Idle,
                    SessionAttention::None,
                    0,
                ),
                summary(
                    "2",
                    Some(&wide),
                    SessionStatus::Idle,
                    SessionAttention::None,
                    0,
                ),
            ],
            NOW,
        );

        // Act
        let shown = screen(110, 8, |frame| picker.render(frame, frame.area()));

        // Assert
        assert!(shown.contains("#1 | faux | just now"), "{shown}");
        assert!(shown.contains("#2 | faux | just now"), "{shown}");
        assert!(!shown.contains(&"w".repeat(41)), "{shown}");
    }

    #[test]
    fn a_row_shows_the_state_title_backend_and_age() {
        // Arrange
        let picker = mixed();

        // Act
        let shown = screen(110, 12, |frame| picker.render(frame, frame.area()));

        // Assert
        assert!(
            shown.contains("approval needed needs a yes #4 | faux | 2m ago"),
            "{shown}"
        );
        assert!(
            shown.contains("input needed    asked a question #3 | faux | 1m ago"),
            "{shown}"
        );
        assert!(
            shown.contains("working         running now #2 | faux | just now"),
            "{shown}"
        );
        assert!(
            shown.contains("idle            (no message yet) #5 | faux | just now"),
            "{shown}"
        );
        assert!(shown.contains("old done #1 | faux | 15m ago"), "{shown}");
    }

    #[test]
    fn typing_narrows_the_list_by_every_word_in_any_part_of_the_row() {
        // Arrange
        let mut picker = mixed();

        // Act
        type_text(&mut picker, "APPROVAL older");
        let by_words = ids(&picker);
        picker.handle_key(press(KeyCode::Backspace));
        picker.handle_key(press(KeyCode::Backspace));
        picker.handle_key(press(KeyCode::Backspace));
        picker.handle_key(press(KeyCode::Backspace));
        picker.handle_key(press(KeyCode::Backspace));
        picker.handle_key(press(KeyCode::Backspace));
        let after_backspace = ids(&picker);

        // Assert
        assert_eq!(by_words, ["6"]);
        assert_eq!(after_backspace, ["4", "6"]);
    }

    #[test]
    fn the_filter_matches_the_folder_the_backend_and_the_id() {
        // Arrange
        let mut by_folder = mixed();
        let mut by_backend = mixed();
        let mut by_id = mixed();

        // Act
        type_text(&mut by_folder, "/work/repo");
        type_text(&mut by_backend, "faux");
        type_text(&mut by_id, "3");

        // Assert
        assert_eq!(by_folder.visible().len(), 6);
        assert_eq!(by_backend.visible().len(), 6);
        assert_eq!(ids(&by_id), ["3"]);
    }

    #[test]
    fn enter_resumes_the_selected_row_and_the_selection_follows_the_arrows() {
        // Arrange
        let mut picker = mixed();

        // Act
        picker.handle_key(press(KeyCode::Down));
        picker.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        picker.handle_key(press(KeyCode::Up));
        let chosen = picker.handle_key(press(KeyCode::Enter));
        for _ in 0..20 {
            picker.handle_key(press(KeyCode::Down));
        }
        let last = picker.handle_key(press(KeyCode::Enter));
        picker.handle_key(press(KeyCode::PageUp));
        picker.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        let top = picker.handle_key(press(KeyCode::Enter));

        // Assert
        assert_eq!(chosen, Action::Resume(SessionId::new("6")));
        assert_eq!(last, Action::Resume(SessionId::new("1")));
        assert_eq!(top, Action::Resume(SessionId::new("4")));
    }

    #[test]
    fn typing_puts_the_selection_back_on_the_first_match() {
        // Arrange
        let mut picker = mixed();
        picker.handle_key(press(KeyCode::Down));
        picker.handle_key(press(KeyCode::Down));

        // Act
        type_text(&mut picker, "e");
        let chosen = picker.handle_key(press(KeyCode::Enter));

        // Assert
        assert_eq!(chosen, Action::Resume(SessionId::new(&ids(&picker)[0])));
    }

    #[test]
    fn enter_with_nothing_showing_does_nothing_and_escape_closes() {
        // Arrange
        let mut picker = mixed();
        type_text(&mut picker, "zzz");

        // Act
        let enter = picker.handle_key(press(KeyCode::Enter));
        let escape = picker.handle_key(press(KeyCode::Esc));
        let shown = screen(110, 8, |frame| picker.render(frame, frame.area()));

        // Assert
        assert_eq!(enter, Action::None);
        assert_eq!(escape, Action::Close);
        assert!(shown.contains("no session matches"), "{shown}");
    }

    #[test]
    fn releases_and_unbound_chords_change_nothing() {
        // Arrange
        let mut picker = mixed();

        // Act
        picker.handle_key(KeyEvent {
            kind: KeyEventKind::Release,
            ..press(KeyCode::Char('x'))
        });
        picker.handle_key(key(KeyCode::Char('x'), KeyModifiers::CONTROL));
        picker.handle_key(key(KeyCode::Char('x'), KeyModifiers::ALT));

        // Assert
        assert_eq!(picker.filter, "");
        assert_eq!(picker.visible().len(), 6);
    }

    #[test]
    fn a_title_with_control_characters_is_drawn_safely() {
        // Arrange
        let picker = Picker::new(
            &[summary(
                "1",
                Some("red \u{1b}[31malert\nsecond line"),
                SessionStatus::Idle,
                SessionAttention::None,
                0,
            )],
            NOW,
        );

        // Act
        let shown = screen(110, 6, |frame| picker.render(frame, frame.area()));

        // Assert
        assert!(!shown.contains('\u{1b}'), "{shown:?}");
        assert!(shown.contains("second line"), "{shown}");
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_selection_in_view() {
        // Arrange
        let sessions: Vec<_> = (0..30)
            .map(|n| {
                summary(
                    &n.to_string(),
                    Some(&format!("task {n:02}")),
                    SessionStatus::Idle,
                    SessionAttention::None,
                    i64::from(n) * 1000,
                )
            })
            .collect();
        let mut picker = Picker::new(&sessions, NOW);
        for _ in 0..25 {
            picker.handle_key(press(KeyCode::Down));
        }

        // Act
        let shown = screen(110, 10, |frame| picker.render(frame, frame.area()));

        // Assert
        assert!(shown.contains("task 25"), "{shown}");
        assert!(!shown.contains("task 00"), "{shown}");
    }

    #[test]
    fn ages_read_in_the_largest_unit_and_never_go_negative() {
        // Arrange / Act / Assert
        assert_eq!(age_text(NOW, NOW), "just now");
        assert_eq!(age_text(NOW, NOW + 5000), "just now");
        assert_eq!(age_text(NOW, NOW - 59_000), "just now");
        assert_eq!(age_text(NOW, NOW - 60_000), "1m ago");
        assert_eq!(age_text(NOW, NOW - 3_600_000), "1h ago");
        assert_eq!(age_text(NOW, NOW - 86_400_000 * 3), "3d ago");
    }

    #[test]
    fn a_tiny_screen_does_not_panic() {
        // Arrange
        let picker = mixed();

        // Act / Assert
        for (width, height) in [(0, 0), (1, 1), (5, 2), (20, 3)] {
            let _ = screen(width, height, |frame| picker.render(frame, frame.area()));
        }
    }
}
