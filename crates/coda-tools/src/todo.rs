// SPDX-License-Identifier: Apache-2.0

//! The `todo` tool: a short ordered list the model keeps for itself.
//!
//! The tool is not a [`Tool`](crate::Tool). The list belongs to the session, is
//! stored as events and is shown to the user, so the turn loop of `coda-core`
//! dispatches it. It is left out of [`TOOL_NAMES`](crate::TOOL_NAMES), so no
//! agent definition may list it, and a sub-agent has no list of its own.

use coda_protocol::events::{TodoItem, TodoStatus};
use serde::Deserialize;
use serde_json::Value;

use crate::{ToolError, parse_args};

/// The name the model uses to call the tool.
pub const NAME: &str = "todo";

/// What the model is told the tool does.
pub const DESCRIPTION: &str = "Keep a short ordered list of the steps of a task. Use action \
`add` with `text` to add a step, `set` with `id` and `status` (pending, in_progress or done) to \
change one, and `clear` to empty the list. The reply is the whole list.";

/// Most items the list holds, so it stays short enough to read at a glance.
pub const MAX_ITEMS: usize = 20;

/// Longest text of one item, in characters.
pub const MAX_TEXT_CHARS: usize = 200;

/// One change to the list.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum TodoOp {
    /// Adds an item at the end, not started.
    Add {
        /// What is to be done.
        text: String,
    },
    /// Changes the status of an item.
    Set {
        /// The item, by its number.
        id: u32,
        /// The new status.
        status: TodoStatus,
    },
    /// Removes every item.
    Clear {},
}

/// Reads the arguments of a `todo` call.
///
/// # Errors
/// Returns [`ToolError::InvalidInput`] when the action is missing or unknown,
/// a field is missing, unknown or of the wrong kind, or the text of an added
/// item is blank or longer than [`MAX_TEXT_CHARS`].
pub fn parse(input: &Value) -> Result<TodoOp, ToolError> {
    let op: TodoOp = parse_args(NAME, input)?;
    if let TodoOp::Add { text } = &op {
        let length = text.trim().chars().count();
        if length == 0 || length > MAX_TEXT_CHARS {
            return Err(invalid(format!(
                "the text must hold 1 to {MAX_TEXT_CHARS} characters"
            )));
        }
    }
    Ok(op)
}

fn invalid(reason: String) -> ToolError {
    ToolError::InvalidInput { tool: NAME, reason }
}

/// The model's todo list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TodoList {
    items: Vec<TodoItem>,
}

impl TodoList {
    /// A list that continues from `items`, as stored.
    #[must_use]
    pub fn from_items(items: Vec<TodoItem>) -> Self {
        Self { items }
    }

    /// The items, in the order they were added.
    #[must_use]
    pub fn items(&self) -> &[TodoItem] {
        &self.items
    }

    /// Applies one change.
    ///
    /// # Errors
    /// Returns [`ToolError::InvalidInput`] when the list is full or `Set` names
    /// an item that is not in it. The list is unchanged then.
    pub fn apply(&mut self, op: TodoOp) -> Result<(), ToolError> {
        match op {
            TodoOp::Add { text } => {
                if self.items.len() >= MAX_ITEMS {
                    return Err(invalid(format!(
                        "the list holds {MAX_ITEMS} items already; finish them or clear it"
                    )));
                }
                let id = self
                    .items
                    .iter()
                    .map(|item| item.id)
                    .max()
                    .map_or(1, |last| last + 1);
                self.items.push(TodoItem {
                    id,
                    text: text.trim().to_owned(),
                    status: TodoStatus::Pending,
                });
            }
            TodoOp::Set { id, status } => {
                let item = self
                    .items
                    .iter_mut()
                    .find(|item| item.id == id)
                    .ok_or_else(|| invalid(format!("there is no item {id}")))?;
                item.status = status;
            }
            TodoOp::Clear {} => self.items.clear(),
        }
        Ok(())
    }

    /// The list as text for the model, one item per line.
    #[must_use]
    pub fn render(&self) -> String {
        render(&self.items)
    }
}

/// `items` as text, one per line, or a note that there are none.
#[must_use]
pub fn render(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "The todo list is empty.".to_owned();
    }
    let lines: Vec<String> = items
        .iter()
        .map(|item| {
            let mark = match item.status {
                TodoStatus::Pending => ' ',
                TodoStatus::InProgress => '~',
                TodoStatus::Done => 'x',
            };
            format!("{}. [{mark}] {}", item.id, item.text)
        })
        .collect();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::TOOL_NAMES;

    fn add(list: &mut TodoList, text: &str) {
        list.apply(TodoOp::Add {
            text: text.to_owned(),
        })
        .unwrap();
    }

    #[test]
    fn the_three_actions_parse_and_anything_else_is_refused() {
        let add = parse(&json!({"action": "add", "text": " write tests "}));
        let set = parse(&json!({"action": "set", "id": 2, "status": "in_progress"}));
        let clear = parse(&json!({"action": "clear"}));

        assert_eq!(
            add.unwrap(),
            TodoOp::Add {
                text: " write tests ".to_owned()
            }
        );
        assert_eq!(
            set.unwrap(),
            TodoOp::Set {
                id: 2,
                status: TodoStatus::InProgress
            }
        );
        assert_eq!(clear.unwrap(), TodoOp::Clear {});
        for refused in [
            json!({}),
            json!({"action": "remove"}),
            json!({"action": "add"}),
            json!({"action": "add", "text": "   "}),
            json!({"action": "add", "text": "x".repeat(MAX_TEXT_CHARS + 1)}),
            json!({"action": "set", "id": 1, "status": "finished"}),
            json!({"action": "clear", "text": "extra"}),
        ] {
            let result = parse(&refused);
            assert!(
                matches!(result, Err(ToolError::InvalidInput { tool: NAME, .. })),
                "{refused}: {result:?}"
            );
        }
        assert!(
            !TOOL_NAMES.contains(&NAME),
            "an agent may not be given todo"
        );
    }

    #[test]
    fn items_are_numbered_in_the_order_they_are_added() {
        let mut list = TodoList::default();
        add(&mut list, "first");
        add(&mut list, "second");

        let ids: Vec<u32> = list.items().iter().map(|item| item.id).collect();

        assert_eq!(ids, [1, 2]);
        assert!(
            list.items()
                .iter()
                .all(|item| item.status == TodoStatus::Pending)
        );
    }

    #[test]
    fn set_changes_one_item_and_an_unknown_id_changes_nothing() {
        let mut list = TodoList::default();
        add(&mut list, "first");
        add(&mut list, "second");

        list.apply(TodoOp::Set {
            id: 2,
            status: TodoStatus::Done,
        })
        .unwrap();
        let before = list.clone();
        let missing = list.apply(TodoOp::Set {
            id: 9,
            status: TodoStatus::Done,
        });

        assert_eq!(list.items()[1].status, TodoStatus::Done);
        assert_eq!(list.items()[0].status, TodoStatus::Pending);
        assert!(matches!(missing, Err(ToolError::InvalidInput { .. })));
        assert_eq!(list, before);
    }

    #[test]
    fn a_full_list_refuses_another_item_and_clear_empties_it() {
        let mut list = TodoList::default();
        for number in 0..MAX_ITEMS {
            add(&mut list, &format!("step {number}"));
        }

        let full = list.apply(TodoOp::Add {
            text: "one more".to_owned(),
        });
        list.apply(TodoOp::Clear {}).unwrap();

        assert!(matches!(full, Err(ToolError::InvalidInput { .. })));
        assert_eq!(list.items().len(), 0);
        assert_eq!(list.render(), "The todo list is empty.");
    }

    #[test]
    fn the_list_renders_one_marked_line_per_item() {
        let mut list = TodoList::default();
        add(&mut list, "plan");
        add(&mut list, "build");
        add(&mut list, "ship");
        list.apply(TodoOp::Set {
            id: 1,
            status: TodoStatus::Done,
        })
        .unwrap();
        list.apply(TodoOp::Set {
            id: 2,
            status: TodoStatus::InProgress,
        })
        .unwrap();

        assert_eq!(list.render(), "1. [x] plan\n2. [~] build\n3. [ ] ship");
    }
}
