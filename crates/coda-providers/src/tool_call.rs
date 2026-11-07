// SPDX-License-Identifier: Apache-2.0

//! Turns what a backend reports about its tool calls into `tool_call` events.

use std::collections::HashMap;

use coda_protocol::events::{Event, ToolCallStatus, summary_of, tool_summary};
use coda_protocol::types::{SessionId, TurnId};
use serde_json::Value;

/// How many calls are remembered between their start and finish. A call past the
/// limit still sends its start, but its finish is not paired and is dropped.
const MAX_OPEN_CALLS: usize = 1024;
const COMMAND_TOOL: &str = "Bash";
const EDIT_TOOL: &str = "Edit";
const WEB_SEARCH_TOOL: &str = "WebSearch";
const MCP_PREFIX: &str = "mcp__";
const MCP_SEPARATOR: &str = "__";
const DECLINED_STATUS: &str = "declined";
const UNKNOWN_TOOL: &str = "unknown";
const FAILED_STATUS: &str = "failed";

/// What a call was named and worked on when it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallInfo {
    pub name: String,
    pub summary: Option<String>,
}

/// The calls a session has started and not finished, so a finish names its tool.
#[derive(Debug)]
pub(crate) struct ToolCalls {
    session_id: SessionId,
    open: HashMap<String, CallInfo>,
}

impl ToolCalls {
    pub(crate) fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            open: HashMap::new(),
        }
    }

    /// Forgets the open calls. A turn that was cut short leaves some behind, and
    /// they would fill the table.
    pub(crate) fn clear(&mut self) {
        self.open.clear();
    }

    /// A `failed` event for every call still open, for a turn that ended without
    /// their results. Empties the table.
    pub(crate) fn fail_open(&mut self, turn_id: &TurnId) -> Vec<Event> {
        let mut open: Vec<_> = self.open.drain().collect();
        open.sort_by(|a, b| a.0.cmp(&b.0));
        open.iter()
            .map(|(id, info)| self.event(turn_id, id, info, ToolCallStatus::Failed))
            .collect()
    }

    /// The start of call `id`; `None` when the call is already open.
    pub(crate) fn started(&mut self, turn_id: &TurnId, id: &str, info: CallInfo) -> Option<Event> {
        if self.open.contains_key(id) {
            return None;
        }
        let event = self.event(turn_id, id, &info, ToolCallStatus::Started);
        if self.open.len() < MAX_OPEN_CALLS {
            self.open.insert(id.to_owned(), info);
        }
        Some(event)
    }

    /// The finish of call `id`; `None` when no start of it was seen.
    pub(crate) fn finished(&mut self, turn_id: &TurnId, id: &str, failed: bool) -> Option<Event> {
        let info = self.open.remove(id)?;
        let status = if failed {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Succeeded
        };
        Some(self.event(turn_id, id, &info, status))
    }

    fn event(&self, turn_id: &TurnId, id: &str, info: &CallInfo, status: ToolCallStatus) -> Event {
        Event::ToolCall {
            session_id: self.session_id.clone(),
            turn_id: turn_id.clone(),
            id: id.to_owned(),
            name: info.name.clone(),
            summary: info.summary.clone(),
            status,
        }
    }
}

/// The name and summary of a Claude Code `tool_use` block.
pub(crate) fn claude_call(block: &Value) -> CallInfo {
    CallInfo {
        name: block["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .unwrap_or(UNKNOWN_TOOL)
            .to_owned(),
        summary: tool_summary(&block["input"]),
    }
}

/// The name and summary of a Codex item, from either `exec --json` (`command_execution`)
/// or the app-server (`commandExecution`). `None` for an item that runs no tool.
pub(crate) fn codex_call(item: &Value) -> Option<CallInfo> {
    let kind = item["type"].as_str()?;
    let plain = |name: &str| CallInfo {
        name: name.to_owned(),
        summary: None,
    };
    Some(match normalized(kind).as_str() {
        "commandexecution" => CallInfo {
            name: COMMAND_TOOL.to_owned(),
            summary: item["commandActions"][0]["command"]
                .as_str()
                .or_else(|| item["command"].as_str())
                .and_then(summary_of),
        },
        "filechange" => CallInfo {
            name: EDIT_TOOL.to_owned(),
            summary: item["changes"][0]["path"].as_str().and_then(summary_of),
        },
        "mcptoolcall" => match (item["server"].as_str(), item["tool"].as_str()) {
            (Some(server), Some(tool)) if !server.is_empty() && !tool.is_empty() => {
                plain(&format!("{MCP_PREFIX}{server}{MCP_SEPARATOR}{tool}"))
            }
            _ => plain(UNKNOWN_TOOL),
        },
        "dynamictoolcall" => plain(item["tool"].as_str().unwrap_or(kind)),
        "websearch" => CallInfo {
            name: WEB_SEARCH_TOOL.to_owned(),
            summary: item["query"].as_str().and_then(summary_of),
        },
        "collabagenttoolcall" | "collabtoolcall" | "imagegeneration" => plain(kind),
        _ => return None,
    })
}

/// Whether a finished Codex item did not succeed: it failed or was declined, or
/// a command exited with a code other than zero.
pub(crate) fn codex_failed(item: &Value) -> bool {
    let status = item["status"].as_str().unwrap_or_default();
    let exit_code = item["exitCode"]
        .as_i64()
        .or_else(|| item["exit_code"].as_i64());
    status == FAILED_STATUS || status == DECLINED_STATUS || exit_code.is_some_and(|code| code != 0)
}

fn normalized(kind: &str) -> String {
    kind.chars()
        .filter(|c| *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn calls() -> ToolCalls {
        ToolCalls::new(SessionId::new("s-1"))
    }

    fn info(name: &str) -> CallInfo {
        CallInfo {
            name: name.to_owned(),
            summary: Some("cargo test".to_owned()),
        }
    }

    fn call_event(id: &str, name: &str, summary: Option<&str>, status: ToolCallStatus) -> Event {
        Event::ToolCall {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            id: id.to_owned(),
            name: name.to_owned(),
            summary: summary.map(str::to_owned),
            status,
        }
    }

    #[test]
    fn a_finish_repeats_the_name_and_summary_of_its_start() {
        // Arrange
        let mut calls = calls();
        let turn = TurnId::new("t-1");

        // Act
        let start = calls.started(&turn, "c1", info("Bash"));
        let finish = calls.finished(&turn, "c1", false);

        // Assert
        let shared = Some("cargo test");
        assert_eq!(
            start,
            Some(call_event("c1", "Bash", shared, ToolCallStatus::Started))
        );
        assert_eq!(
            finish,
            Some(call_event("c1", "Bash", shared, ToolCallStatus::Succeeded))
        );
    }

    #[test]
    fn a_failed_finish_is_marked_failed() {
        // Arrange
        let mut calls = calls();
        let turn = TurnId::new("t-1");
        calls.started(&turn, "c1", info("Bash"));

        // Act
        let finish = calls.finished(&turn, "c1", true);

        // Assert
        assert_eq!(
            finish,
            Some(call_event(
                "c1",
                "Bash",
                Some("cargo test"),
                ToolCallStatus::Failed
            ))
        );
    }

    #[test]
    fn a_repeated_start_and_an_unmatched_finish_send_nothing() {
        // Arrange
        let mut calls = calls();
        let turn = TurnId::new("t-1");
        calls.started(&turn, "c1", info("Bash"));

        // Act and Assert
        assert!(calls.started(&turn, "c1", info("Bash")).is_none());
        assert!(calls.finished(&turn, "other", false).is_none());
        assert!(calls.finished(&turn, "c1", false).is_some());
        assert!(calls.finished(&turn, "c1", false).is_none());
    }

    #[test]
    fn calls_past_the_limit_start_but_are_not_remembered() {
        // Arrange
        let mut calls = calls();
        let turn = TurnId::new("t-1");
        for n in 0..MAX_OPEN_CALLS {
            calls.started(&turn, &format!("c{n}"), info("Bash"));
        }

        // Act
        let start = calls.started(&turn, "extra", info("Bash"));
        let finish = calls.finished(&turn, "extra", false);

        // Assert
        assert!(start.is_some());
        assert!(finish.is_none());
        assert_eq!(calls.open.len(), MAX_OPEN_CALLS);
    }

    #[test]
    fn clearing_forgets_open_calls_so_a_later_turn_starts_fresh() {
        // Arrange
        let mut calls = calls();
        let turn = TurnId::new("t-1");
        calls.started(&turn, "c1", info("Bash"));

        // Act
        calls.clear();

        // Assert
        assert!(calls.finished(&turn, "c1", false).is_none());
        assert!(calls.started(&turn, "c1", info("Bash")).is_some());
    }

    #[test]
    fn open_calls_end_failed_in_id_order_and_the_table_empties() {
        // Arrange
        let mut calls = calls();
        let turn = TurnId::new("t-1");
        calls.started(&turn, "b", info("Bash"));
        calls.started(&turn, "a", info("Bash"));
        calls.started(&turn, "c", info("Bash"));
        calls.finished(&turn, "c", false);

        // Act
        let ended = calls.fail_open(&turn);

        // Assert
        let ids: Vec<_> = ended
            .iter()
            .map(|e| match e {
                Event::ToolCall { id, status, .. } => (id.as_str(), *status),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            ids,
            [("a", ToolCallStatus::Failed), ("b", ToolCallStatus::Failed)]
        );
        assert_eq!(calls.fail_open(&turn).len(), 0);
    }

    #[test]
    fn a_nameless_tool_gets_a_fixed_name() {
        assert_eq!(claude_call(&json!({"name":""})).name, "unknown");
        assert_eq!(claude_call(&json!({})).name, "unknown");
        let mcp = json!({"type":"mcpToolCall","server":"","tool":"x"});
        assert_eq!(codex_call(&mcp).unwrap().name, "unknown");
    }

    #[test]
    fn a_claude_block_is_named_and_summarized_from_its_input() {
        // Arrange
        let block = json!({"type":"tool_use","id":"t1","name":"Edit",
            "input":{"file_path":"src/a.rs","old_string":"x"}});

        // Act
        let call = claude_call(&block);

        // Assert
        assert_eq!(call.name, "Edit");
        assert_eq!(call.summary.as_deref(), Some("src/a.rs"));
        assert_eq!(claude_call(&json!({"name":"Bash"})).summary, None);
    }

    #[test]
    fn a_codex_command_reads_the_same_in_both_protocols() {
        // Arrange
        let exec = json!({"type":"command_execution","command":"/bin/zsh -lc 'ls'"});
        let server = json!({"type":"commandExecution","command":"/bin/zsh -lc 'touch a'",
            "commandActions":[{"command":"touch a","type":"unknown"}]});

        // Act
        let from_exec = codex_call(&exec).unwrap();
        let from_server = codex_call(&server).unwrap();

        // Assert
        assert_eq!(from_exec.name, "Bash");
        assert_eq!(from_exec.summary.as_deref(), Some("/bin/zsh -lc 'ls'"));
        assert_eq!(from_server.name, "Bash");
        assert_eq!(from_server.summary.as_deref(), Some("touch a"));
    }

    #[test]
    fn codex_items_map_to_the_tool_names_claude_uses() {
        // Arrange
        let cases = [
            (
                json!({"type":"fileChange","changes":[{"path":"a.rs"}]}),
                "Edit",
                Some("a.rs"),
            ),
            (
                json!({"type":"file_change","changes":[{"path":"b.rs"}]}),
                "Edit",
                Some("b.rs"),
            ),
            (
                json!({"type":"mcpToolCall","server":"fs","tool":"read"}),
                "mcp__fs__read",
                None,
            ),
            (
                json!({"type":"mcp_tool_call","server":"fs","tool":"read"}),
                "mcp__fs__read",
                None,
            ),
            (
                json!({"type":"webSearch","query":"rust"}),
                "WebSearch",
                Some("rust"),
            ),
            (
                json!({"type":"dynamicToolCall","tool":"lookup"}),
                "lookup",
                None,
            ),
        ];

        for (item, name, summary) in cases {
            // Act
            let call = codex_call(&item).unwrap();

            // Assert
            assert_eq!(
                (call.name.as_str(), call.summary.as_deref()),
                (name, summary),
                "{item}"
            );
        }
    }

    #[test]
    fn a_codex_item_that_runs_no_tool_is_not_a_call() {
        assert!(codex_call(&json!({"type":"agentMessage"})).is_none());
        assert!(codex_call(&json!({"type":"reasoning"})).is_none());
        assert!(codex_call(&json!({"id":"x"})).is_none());
    }

    #[test]
    fn a_codex_item_fails_by_status_decline_or_exit_code() {
        assert!(!codex_failed(&json!({"status":"completed","exitCode":0})));
        assert!(!codex_failed(&json!({"status":"completed"})));
        assert!(codex_failed(&json!({"status":"failed"})));
        assert!(codex_failed(&json!({"status":"declined"})));
        assert!(codex_failed(&json!({"status":"completed","exit_code":2})));
        assert!(codex_failed(&json!({"status":"completed","exitCode":1})));
    }
}
