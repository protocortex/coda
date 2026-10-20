// SPDX-License-Identifier: AGPL-3.0-only

//! The transcript: what the user said and what the model streamed back.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::Arc;

use coda_protocol::events::Event;
use coda_protocol::types::TurnId;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const USER_PREFIX: &str = "you: ";
const NOTICE_PREFIX: &str = "! ";
const DROPPED_MARKER: &str = "[older messages dropped]";
/// Entries kept; the oldest go first.
const MAX_ENTRIES: usize = 1000;
/// Text bytes kept across all entries; the newest entry always stays.
const MAX_BYTES: usize = 1024 * 1024;
const TAB_WIDTH: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    User(String),
    Assistant { turn: TurnId, text: String },
    Notice(String),
}

impl Entry {
    fn text_len(&self) -> usize {
        match self {
            Self::User(text) | Self::Notice(text) | Self::Assistant { text, .. } => text.len(),
        }
    }

    fn display(&self) -> String {
        match self {
            Self::User(text) => format!("{USER_PREFIX}{text}"),
            Self::Assistant { text, .. } => text.clone(),
            Self::Notice(text) => format!("{NOTICE_PREFIX}{text}"),
        }
    }
}

/// An entry with its rows wrapped for one width, kept until either changes.
#[derive(Debug)]
struct Slot {
    entry: Entry,
    wrapped: RefCell<Option<(usize, Arc<[String]>)>>,
}

impl Slot {
    fn new(entry: Entry) -> Self {
        Self {
            entry,
            wrapped: RefCell::new(None),
        }
    }

    fn rows(&self, width: usize, wraps: &Cell<usize>) -> Arc<[String]> {
        let mut cache = self.wrapped.borrow_mut();
        match &*cache {
            Some((cached, rows)) if *cached == width => Arc::clone(rows),
            _ => {
                wraps.set(wraps.get() + 1);
                let rows: Arc<[String]> = wrap_rows(&self.entry.display(), width).into();
                *cache = Some((width, Arc::clone(&rows)));
                rows
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct Chat {
    entries: VecDeque<Slot>,
    /// Text bytes held by `entries`.
    bytes: usize,
    dropped: bool,
    /// First visible row while the user is scrolled up; `None` follows the newest row.
    top: Cell<Option<usize>>,
    /// The highest `top` of the last draw, which scrolling measures from.
    max_top: Cell<usize>,
    wraps: Cell<usize>,
}

impl Chat {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.push(Entry::User(text.into()));
        self.top.set(None);
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.push(Entry::Notice(text.into()));
        self.top.set(None);
    }

    /// Forgets everything shown, for a different session.
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// Streamed text extends the message of its turn; crashes and interruptions
    /// leave a notice; every other event has nothing to show.
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::StreamDelta { turn_id, text, .. } => match self.entries.back_mut() {
                Some(Slot {
                    entry: Entry::Assistant { turn, text: shown },
                    wrapped,
                }) if turn == turn_id => {
                    shown.push_str(text);
                    *wrapped.get_mut() = None;
                    self.bytes += text.len();
                    self.trim();
                }
                _ => self.push(Entry::Assistant {
                    turn: turn_id.clone(),
                    text: text.clone(),
                }),
            },
            Event::BackendCrashed {
                exit_code,
                will_restart,
                detail,
                ..
            } => {
                let mut notice = match exit_code {
                    Some(code) => format!("backend crashed (exit {code})"),
                    None => "backend crashed".to_owned(),
                };
                // The last line is the reason; earlier ones are usually a banner.
                if let Some(reason) = detail.as_deref().and_then(|text| text.lines().last()) {
                    notice.push_str(": ");
                    notice.push_str(reason);
                }
                if *will_restart {
                    notice.push_str(", restarting");
                }
                self.push(Entry::Notice(notice));
            }
            Event::TurnInterrupted { .. } => {
                self.push(Entry::Notice("turn interrupted".to_owned()));
            }
            _ => {}
        }
    }

    fn push(&mut self, entry: Entry) {
        self.bytes += entry.text_len();
        self.entries.push_back(Slot::new(entry));
        self.trim();
    }

    fn trim(&mut self) {
        while self.entries.len() > 1 && (self.entries.len() > MAX_ENTRIES || self.bytes > MAX_BYTES)
        {
            if let Some(oldest) = self.entries.pop_front() {
                self.bytes -= oldest.entry.text_len();
                self.dropped = true;
            }
        }
    }

    /// Moves the view toward older rows. The first move pins the view to the
    /// rows on screen, so rows that arrive later do not shift it.
    pub fn scroll_up(&mut self, lines: usize) {
        let top = self.top.get().unwrap_or_else(|| self.max_top.get());
        self.top.set(Some(top.saturating_sub(lines)));
    }

    /// Moves toward newer rows; reaching the newest ones follows them again.
    pub fn scroll_down(&mut self, lines: usize) {
        if let Some(top) = self.top.get() {
            let top = top.saturating_add(lines);
            self.top.set((top < self.max_top.get()).then_some(top));
        }
    }

    /// Draws the newest rows that fit, or the pinned ones when scrolled up.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let width = usize::from(area.width);
        let height = usize::from(area.height);
        let marker: Arc<[String]> = if self.dropped {
            wrap_rows(DROPPED_MARKER, width).into()
        } else {
            Arc::default()
        };
        let blocks: Vec<Arc<[String]>> = std::iter::once(marker)
            .chain(
                self.entries
                    .iter()
                    .map(|slot| slot.rows(width, &self.wraps)),
            )
            .collect();
        let total: usize = blocks.iter().map(|block| block.len()).sum();
        let max_top = total.saturating_sub(height);
        self.max_top.set(max_top);
        let top = match self.top.get() {
            Some(top) if top < max_top => top,
            _ => {
                self.top.set(None);
                max_top
            }
        };
        let lines: Vec<Line> = blocks
            .iter()
            .flat_map(|block| block.iter())
            .skip(top)
            .take(height)
            .map(|row| Line::raw(row.as_str()))
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }
}

/// Splits `text` into rows no wider than `width` display columns, measuring
/// whole grapheme clusters so emoji sequences are never cut. A cluster wider
/// than `width` gets a row of its own so the split always makes progress.
/// Tabs become spaces and other control characters are dropped, so model
/// output can never reach the terminal as an escape sequence.
fn wrap_rows(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let line = printable(line);
        let mut row = String::new();
        let mut used = 0;
        for cluster in line.graphemes(true) {
            let columns = cluster.width();
            if used + columns > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push_str(cluster);
            used += columns;
        }
        rows.push(row);
    }
    rows
}

fn printable(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for ch in line.chars() {
        match ch {
            '\t' => out.extend(std::iter::repeat_n(' ', TAB_WIDTH)),
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use coda_protocol::types::{SessionId, TurnId};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::test_support::screen;

    const WIDTH: u16 = 20;
    const HEIGHT: u16 = 4;

    fn delta(turn: &str, text: &str) -> Event {
        Event::StreamDelta {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new(turn),
            text: text.to_owned(),
        }
    }

    fn draw(chat: &Chat, width: u16, height: u16) -> String {
        screen(width, height, |frame| chat.render(frame, frame.area()))
    }

    #[test]
    fn streamed_deltas_of_one_turn_join_into_one_message() {
        // Arrange
        let mut chat = Chat::new();

        // Act
        chat.apply(&delta("t-1", "Hel"));
        chat.apply(&delta("t-1", "lo"));

        // Assert
        let shown = draw(&chat, WIDTH, HEIGHT);
        assert!(shown.contains("Hello"), "{shown}");
    }

    #[test]
    fn a_new_turn_starts_a_new_message() {
        // Arrange
        let mut chat = Chat::new();

        // Act
        chat.apply(&delta("t-1", "first"));
        chat.apply(&delta("t-2", "second"));

        // Assert
        let rows: Vec<_> = draw(&chat, WIDTH, HEIGHT)
            .lines()
            .map(|row| row.trim_end().to_owned())
            .collect();
        assert!(rows.contains(&"first".to_owned()), "{rows:?}");
        assert!(rows.contains(&"second".to_owned()), "{rows:?}");
    }

    #[test]
    fn user_text_is_marked_and_shown_before_the_reply() {
        // Arrange
        let mut chat = Chat::new();

        // Act
        chat.push_user("hi");
        chat.apply(&delta("t-1", "reply"));

        // Assert
        let shown = draw(&chat, WIDTH, HEIGHT);
        let user = shown.find("you: hi").expect(&shown);
        let reply = shown.find("reply").expect(&shown);
        assert!(user < reply, "{shown}");
    }

    #[test]
    fn a_crash_and_an_interruption_leave_a_notice() {
        for (event, expected) in [
            (
                Event::BackendCrashed {
                    session_id: SessionId::new("s-1"),
                    exit_code: Some(2),
                    will_restart: false,
                    detail: None,
                },
                "backend crashed (exit 2)",
            ),
            (
                Event::BackendCrashed {
                    session_id: SessionId::new("s-1"),
                    exit_code: Some(1),
                    will_restart: false,
                    detail: Some("banner\nno repo".to_owned()),
                },
                "backend crashed (exit 1): no repo",
            ),
            (
                Event::TurnInterrupted {
                    session_id: SessionId::new("s-1"),
                    turn_id: TurnId::new("t-1"),
                },
                "turn interrupted",
            ),
        ] {
            // Arrange
            let mut chat = Chat::new();

            // Act
            chat.apply(&event);

            // Assert
            let shown = draw(&chat, 40, HEIGHT);
            assert!(shown.contains(expected), "{shown}");
        }
    }

    #[test]
    fn events_without_text_do_not_change_the_transcript() {
        // Arrange
        let mut chat = Chat::new();

        // Act
        chat.apply(&Event::Result {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            is_error: false,
        });

        // Assert
        assert_eq!(draw(&chat, WIDTH, HEIGHT).trim(), "");
    }

    #[test]
    fn long_text_wraps_and_the_latest_rows_stay_visible() {
        // Arrange
        let mut chat = Chat::new();
        let text: String = ('a'..='z').collect();

        // Act
        chat.apply(&delta("t-1", &text));

        // Assert
        let shown = draw(&chat, 10, 2);
        assert_eq!(shown, "klmnopqrst\nuvwxyz    ");
    }

    #[test]
    fn scrolling_up_reveals_older_rows_and_is_bounded() {
        // Arrange
        let mut chat = Chat::new();
        chat.apply(&delta("t-1", "one\ntwo\nthree"));

        // Act
        chat.scroll_up(1);
        let one_up = draw(&chat, 10, 2);
        chat.scroll_up(1000);
        let top = draw(&chat, 10, 2);
        chat.scroll_down(1000);
        let bottom = draw(&chat, 10, 2);

        // Assert
        assert_eq!(one_up, "one       \ntwo       ");
        assert_eq!(top, "one       \ntwo       ");
        assert_eq!(bottom, "two       \nthree     ");
    }

    #[test]
    fn wrapping_counts_display_columns_not_chars() {
        // Arrange
        let text = "ab\u{4e2d}\u{6587}cd";

        // Act
        let rows = wrap_rows(text, 4);

        // Assert
        assert_eq!(rows, vec!["ab\u{4e2d}", "\u{6587}cd"]);
    }

    #[test]
    fn a_char_wider_than_the_width_still_makes_progress() {
        // Arrange / Act
        let rows = wrap_rows("\u{4e2d}\u{6587}", 1);

        // Assert
        assert_eq!(rows, vec!["\u{4e2d}", "\u{6587}"]);
    }

    fn assert_fits(text: &str, width: usize) {
        for row in wrap_rows(text, width) {
            assert!(row.width() <= width, "{row:?} wider than {width}");
        }
    }

    #[test]
    fn tabs_expand_to_spaces_before_wrapping() {
        // Arrange / Act
        let rows = wrap_rows("\tfn main()", 80);

        // Assert
        assert_eq!(rows, vec![format!("{}fn main()", " ".repeat(TAB_WIDTH))]);
        assert_fits("\t\t\tindented code\twith tabs", 7);
    }

    #[test]
    fn grapheme_clusters_never_exceed_the_width() {
        // Arrange
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        let heart = "\u{2764}\u{fe0f}";
        let texts = [
            family.repeat(6),
            heart.repeat(9),
            format!("a{heart}{family}b"),
        ];

        // Act / Assert
        for text in &texts {
            for width in [2, 3, 5, 8] {
                assert_fits(text, width);
            }
        }
        assert_eq!(wrap_rows(&family.repeat(3), 4).len(), 2);
    }

    #[test]
    fn model_control_sequences_render_as_inert_text() {
        // Arrange
        let mut chat = Chat::new();
        let hostile = "\x1b[2J\x1b]0;pwned\x07\x1b]8;;http://x\x1b\\link\u{9b}31m\r\x08\x7f ok";

        // Act
        chat.apply(&delta("t-1", hostile));
        let mut terminal = Terminal::new(TestBackend::new(60, 4)).unwrap();
        terminal
            .draw(|frame| chat.render(frame, frame.area()))
            .unwrap();

        // Assert
        let cells = terminal.backend().buffer().content();
        assert!(
            cells
                .iter()
                .all(|cell| !cell.symbol().chars().any(char::is_control))
        );
        let shown = draw(&chat, 60, 4);
        assert!(shown.contains("ok") && shown.contains("link"), "{shown}");
    }

    #[test]
    fn old_entries_are_dropped_past_the_entry_cap_with_a_marker() {
        // Arrange
        let mut chat = Chat::new();

        // Act
        for n in 0..MAX_ENTRIES + 5 {
            chat.push_user(format!("m{n}"));
        }

        // Assert
        assert_eq!(chat.entries.len(), MAX_ENTRIES);
        let bottom = draw(&chat, 30, 2);
        assert!(
            bottom.contains(&format!("m{}", MAX_ENTRIES + 4)),
            "{bottom}"
        );
        chat.scroll_up(usize::MAX);
        let top = draw(&chat, 30, 3);
        assert!(top.starts_with(DROPPED_MARKER), "{top}");
        assert!(top.contains("m5"), "{top}");
        assert!(!top.contains("m4 "), "{top}");
    }

    #[test]
    fn old_entries_are_dropped_past_the_byte_cap_but_the_newest_is_kept() {
        // Arrange
        let mut chat = Chat::new();
        let big = "x".repeat(MAX_BYTES / 2 + 1);

        // Act
        chat.apply(&delta("t-1", &big));
        chat.apply(&delta("t-2", &big));
        chat.apply(&delta("t-3", &big));

        // Assert
        assert_eq!(chat.entries.len(), 1);
        assert!(chat.bytes <= big.len());
    }

    #[test]
    fn redrawing_an_unchanged_transcript_does_not_wrap_again() {
        // Arrange
        let mut chat = Chat::new();
        chat.push_user("hello");
        chat.apply(&delta("t-1", "world"));
        draw(&chat, 20, 4);
        let first = chat.wraps.get();

        // Act
        draw(&chat, 20, 4);
        let unchanged = chat.wraps.get();
        chat.apply(&delta("t-1", "!"));
        draw(&chat, 20, 4);
        let one_changed = chat.wraps.get();
        draw(&chat, 10, 4);

        // Assert
        assert_eq!(first, 2);
        assert_eq!(unchanged, first);
        assert_eq!(one_changed, first + 1);
        assert_eq!(chat.wraps.get(), one_changed + 2);
    }

    #[test]
    fn a_scrolled_view_stays_put_while_new_rows_stream_in() {
        // Arrange
        let mut chat = Chat::new();
        chat.apply(&delta("t-1", "a\nb\nc\nd\ne"));
        draw(&chat, 10, 2);
        chat.scroll_up(1);
        let before = draw(&chat, 10, 2);

        // Act
        chat.apply(&delta("t-1", "\nf\ng"));
        let anchored = draw(&chat, 10, 2);
        chat.scroll_down(usize::MAX);
        let bottom = draw(&chat, 10, 2);
        chat.apply(&delta("t-1", "\nh"));
        let following = draw(&chat, 10, 2);

        // Assert
        assert_eq!(before, "c         \nd         ");
        assert_eq!(anchored, before);
        assert_eq!(bottom, "f         \ng         ");
        assert_eq!(following, "g         \nh         ");
    }

    #[test]
    fn blank_lines_are_kept_and_zero_width_yields_nothing() {
        // Arrange / Act / Assert
        assert_eq!(wrap_rows("a\n\nb", 5), vec!["a", "", "b"]);
        assert_eq!(wrap_rows("abc", 0), Vec::<String>::new());
    }

    #[test]
    fn tiny_and_empty_screens_do_not_panic() {
        // Arrange
        let mut chat = Chat::new();
        let empty = draw(&chat, 1, 1);
        chat.push_user("hello wide \u{4e2d}\u{6587} world");
        chat.apply(&delta("t-1", &"x".repeat(500)));

        // Act
        let sizes = [(1, 1), (1, 5), (5, 1), (0, 0), (3, 2)];

        // Assert
        assert_eq!(empty.trim(), "");
        for (width, height) in sizes {
            let _ = draw(&chat, width, height);
        }
    }
}
