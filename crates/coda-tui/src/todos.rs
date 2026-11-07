// SPDX-License-Identifier: Apache-2.0

//! The model's todo list as the lines drawn above the prompt.

use coda_protocol::events::{TodoItem, TodoStatus};

use crate::permission::sanitize_line;

/// Most item lines drawn; the rest are counted in one more line.
pub const MAX_ROWS: usize = 6;

/// The lines for `items`, one per item up to [`MAX_ROWS`] and then one that
/// counts the rest. Empty when there are no items.
#[must_use]
pub fn lines(items: &[TodoItem]) -> Vec<String> {
    let mut lines: Vec<String> = items.iter().take(MAX_ROWS).map(line).collect();
    if items.len() > MAX_ROWS {
        lines.push(format!("... and {} more", items.len() - MAX_ROWS));
    }
    lines
}

fn line(item: &TodoItem) -> String {
    let mark = match item.status {
        TodoStatus::Pending => ' ',
        TodoStatus::InProgress => '~',
        TodoStatus::Done => 'x',
    };
    format!("[{mark}] {}", sanitize_line(&item.text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u32, text: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            id,
            text: text.to_owned(),
            status,
        }
    }

    #[test]
    fn each_item_is_one_line_marked_by_its_status() {
        let items = [
            item(1, "plan", TodoStatus::Done),
            item(2, "build", TodoStatus::InProgress),
            item(3, "ship", TodoStatus::Pending),
        ];

        assert_eq!(lines(&items), ["[x] plan", "[~] build", "[ ] ship"]);
    }

    #[test]
    fn a_long_list_is_cut_and_the_rest_counted() {
        let items: Vec<TodoItem> = (1..=9)
            .map(|id| item(id, "step", TodoStatus::Pending))
            .collect();

        let drawn = lines(&items);

        assert_eq!(drawn.len(), MAX_ROWS + 1);
        assert_eq!(drawn[MAX_ROWS], "... and 3 more");
    }

    #[test]
    fn control_bytes_in_an_item_never_reach_the_screen() {
        let items = [item(1, "red\u{1b}[31m\ntext", TodoStatus::Pending)];

        let drawn = lines(&items);

        assert!(!drawn[0].chars().any(char::is_control), "{drawn:?}");
    }

    #[test]
    fn no_items_draw_nothing() {
        assert_eq!(lines(&[]), Vec::<String>::new());
    }
}
