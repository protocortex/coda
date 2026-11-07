// SPDX-License-Identifier: Apache-2.0

//! What the agent did in a turn, and the words that describe it.
//!
//! This module has no terminal code. It reads `tool_call` events and returns
//! text: the live line while a tool runs, and the one line a settled turn folds
//! into. The caller owns the clock and passes it in, so every rule here can be
//! tested without waiting.

use std::collections::HashSet;
use std::fmt::Write;
use std::time::{Duration, Instant};

use coda_protocol::events::{Event, ToolCallStatus};
use coda_protocol::types::TurnId;

const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 3600;
/// At most this many categories are named in a fold.
const MAX_FOLD_CATEGORIES: usize = 2;
const WAITING_FOR_APPROVAL: &str = "waiting for approval";
const COMMAND_TOOLS: [&str; 1] = ["bash"];
const EDIT_TOOLS: [&str; 6] = [
    "edit",
    "write",
    "multiedit",
    "multi_edit",
    "notebookedit",
    "apply_patch",
];
const READ_TOOLS: [&str; 11] = [
    "read",
    "grep",
    "glob",
    "list",
    "ls",
    "notebookread",
    "retrieve",
    "code_outline",
    "code_read_range",
    "code_search",
    "code_symbol",
];

/// How the work of a turn is grouped in the fold, most important first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Shell commands.
    Command,
    /// Edits and writes of files.
    Edit,
    /// Reads, searches and listings.
    Read,
    /// Anything else, such as a web search or an MCP tool. Never counted in the fold.
    Other,
}

/// The category of a tool by its name. Backends spell the same tool in different
/// cases (`Bash` and `bash`), so the match ignores case.
#[must_use]
pub fn category(name: &str) -> Category {
    let name = name.to_lowercase();
    let name = name.as_str();
    if COMMAND_TOOLS.contains(&name) {
        Category::Command
    } else if EDIT_TOOLS.contains(&name) {
        Category::Edit
    } else if READ_TOOLS.contains(&name) {
        Category::Read
    } else {
        Category::Other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Open,
    Succeeded,
    Failed,
}

/// One call of the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Call {
    id: String,
    name: String,
    summary: Option<String>,
    started: Instant,
    state: State,
}

impl Call {
    fn target(&self) -> &str {
        self.summary.as_deref().unwrap_or(&self.name)
    }
}

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The turn ended with a result.
    Done,
    /// The turn ended with a result that is an error.
    Failed,
    /// The turn was cut short, or its backend was lost.
    Stopped,
}

/// The line a settled turn folds into, and the calls behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fold {
    /// The one-line summary.
    pub line: String,
    /// One line per call, shown when the user expands the fold.
    pub details: Vec<String>,
}

/// The work of the turn in progress.
#[derive(Debug, Default)]
pub struct WorkLog {
    turn: Option<TurnId>,
    began: Option<Instant>,
    calls: Vec<Call>,
}

impl WorkLog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Notes that a turn began at `now`, when the user sent a message.
    pub fn begin(&mut self, now: Instant) {
        self.reset();
        self.began = Some(now);
    }

    /// Forgets the turn, for a different session or a lost link.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// When the turn began, if one is running.
    #[must_use]
    pub fn began(&self) -> Option<Instant> {
        self.began
    }

    /// Reads one event. Returns the fold when the event settles the turn.
    pub fn apply(&mut self, event: &Event, now: Instant) -> Option<Fold> {
        match event {
            Event::ToolCall {
                turn_id,
                id,
                name,
                summary,
                status,
                ..
            } => self.apply_call(turn_id, id, name, summary.as_ref(), *status, now),
            Event::Result { is_error, .. } => {
                let outcome = if *is_error {
                    Outcome::Failed
                } else {
                    Outcome::Done
                };
                return self.settle(outcome, now);
            }
            Event::TurnInterrupted { .. } => return self.settle(Outcome::Stopped, now),
            Event::BackendCrashed { will_restart, .. } if !will_restart => {
                return self.settle(Outcome::Stopped, now);
            }
            _ => {}
        }
        None
    }

    fn apply_call(
        &mut self,
        turn_id: &TurnId,
        id: &str,
        name: &str,
        summary: Option<&String>,
        status: ToolCallStatus,
        now: Instant,
    ) {
        if self.turn.as_ref().is_some_and(|turn| turn != turn_id) {
            self.reset();
        }
        self.turn = Some(turn_id.clone());
        self.began.get_or_insert(now);
        let state = match status {
            ToolCallStatus::Started => State::Open,
            ToolCallStatus::Succeeded => State::Succeeded,
            ToolCallStatus::Failed => State::Failed,
            _ => return,
        };
        if let Some(call) = self.calls.iter_mut().find(|call| call.id == id) {
            call.state = state;
        } else {
            self.calls.push(Call {
                id: id.to_owned(),
                name: name.to_owned(),
                summary: summary.cloned(),
                started: now,
                state,
            });
        }
    }

    fn settle(&mut self, outcome: Outcome, now: Instant) -> Option<Fold> {
        let elapsed = self.began.map(|began| now.saturating_duration_since(began));
        let fold = fold(&self.calls, outcome, elapsed.unwrap_or_default());
        self.reset();
        fold
    }

    /// The live line: the tool that runs now with its time, or that a prompt
    /// waits for an answer. `None` when nothing runs.
    #[must_use]
    pub fn live(&self, now: Instant, awaiting_approval: bool) -> Option<String> {
        if awaiting_approval {
            return Some(WAITING_FOR_APPROVAL.to_owned());
        }
        let open: Vec<_> = self
            .calls
            .iter()
            .filter(|call| call.state == State::Open)
            .collect();
        let newest = open.last()?;
        let verb = match category(&newest.name) {
            Category::Edit => "editing",
            Category::Read => "reading",
            Category::Command | Category::Other => "running",
        };
        let mut line = format!("{verb} {}", newest.target());
        let time = format_duration(now.saturating_duration_since(newest.started));
        if !time.is_empty() {
            line.push(' ');
            line.push_str(&time);
        }
        if open.len() > 1 {
            let _ = write!(line, " (+{} more)", open.len() - 1);
        }
        Some(line)
    }
}

fn fold(calls: &[Call], outcome: Outcome, elapsed: Duration) -> Option<Fold> {
    let time = format_duration(elapsed);
    let prefix = match (outcome, time.is_empty()) {
        (Outcome::Done, true) => "Worked".to_owned(),
        (Outcome::Done, false) => format!("Worked for {time}"),
        (Outcome::Failed, true) => "Failed".to_owned(),
        (Outcome::Failed, false) => format!("Failed after {time}"),
        (Outcome::Stopped, true) => "Stopped".to_owned(),
        (Outcome::Stopped, false) => format!("Stopped after {time}"),
    };
    let body = [Category::Command, Category::Edit, Category::Read]
        .into_iter()
        .filter_map(|wanted| describe(calls, wanted))
        .take(MAX_FOLD_CATEGORIES)
        .collect::<Vec<_>>()
        .join(" and ");
    let cut = calls
        .iter()
        .filter(|call| call.state == State::Open && category(&call.name) != Category::Other)
        .count();
    let line = match (body.is_empty(), cut) {
        (true, 0) => return None,
        (true, cut) => format!("{prefix}: {cut} cut short"),
        (false, 0) => format!("{prefix}: {body}"),
        (false, cut) => format!("{prefix}: {body}, {cut} cut short"),
    };
    let details = calls
        .iter()
        .map(|call| {
            let mark = match call.state {
                State::Open => " (cut short)",
                State::Failed => " (failed)",
                State::Succeeded => "",
            };
            match &call.summary {
                Some(summary) => format!("{}: {summary}{mark}", call.name),
                None => format!("{}{mark}", call.name),
            }
        })
        .collect();
    Some(Fold { line, details })
}

/// "ran 4 commands (1 failed)" for the finished calls of one category.
fn describe(calls: &[Call], wanted: Category) -> Option<String> {
    let of_kind: Vec<_> = calls
        .iter()
        .filter(|call| call.state != State::Open && category(&call.name) == wanted)
        .collect();
    if of_kind.is_empty() {
        return None;
    }
    let count = if wanted == Category::Command {
        of_kind.len()
    } else {
        of_kind
            .iter()
            .map(|call| call.summary.as_deref().unwrap_or(&call.id))
            .collect::<HashSet<_>>()
            .len()
    };
    let (verb, one, many) = match wanted {
        Category::Command => ("ran", "command", "commands"),
        Category::Edit => ("edited", "file", "files"),
        _ => ("read", "item", "items"),
    };
    let noun = if count == 1 { one } else { many };
    let failed = of_kind
        .iter()
        .filter(|call| call.state == State::Failed)
        .count();
    let mark = if failed > 0 {
        format!(" ({failed} failed)")
    } else {
        String::new()
    };
    Some(format!("{verb} {count} {noun}{mark}"))
}

/// A duration as short text: `12s`, `1m 04s`, `1h 05m`. Under a second it is empty.
#[must_use]
pub fn format_duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds == 0 {
        String::new()
    } else if seconds < SECONDS_PER_MINUTE {
        format!("{seconds}s")
    } else if seconds < SECONDS_PER_HOUR {
        format!(
            "{}m {:02}s",
            seconds / SECONDS_PER_MINUTE,
            seconds % SECONDS_PER_MINUTE
        )
    } else {
        format!(
            "{}h {:02}m",
            seconds / SECONDS_PER_HOUR,
            seconds % SECONDS_PER_HOUR / SECONDS_PER_MINUTE
        )
    }
}

#[cfg(test)]
mod tests {
    use coda_protocol::types::SessionId;

    use super::*;

    fn call(id: &str, name: &str, summary: Option<&str>, status: ToolCallStatus) -> Event {
        Event::ToolCall {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            id: id.to_owned(),
            name: name.to_owned(),
            summary: summary.map(str::to_owned),
            status,
        }
    }

    fn result(is_error: bool) -> Event {
        Event::Result {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            is_error,
        }
    }

    fn interrupted() -> Event {
        Event::TurnInterrupted {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            cause: None,
        }
    }

    /// Runs `calls` as (id, name, summary, ends failed) and settles with `last`.
    fn run(calls: &[(&str, &str, Option<&str>, bool)], last: &Event, took: u64) -> Option<Fold> {
        let t0 = Instant::now();
        let mut log = WorkLog::new();
        log.begin(t0);
        for (id, name, summary, failed) in calls {
            log.apply(&call(id, name, *summary, ToolCallStatus::Started), t0);
            let end = if *failed {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Succeeded
            };
            log.apply(&call(id, name, *summary, end), t0);
        }
        log.apply(last, t0 + Duration::from_secs(took))
    }

    #[test]
    fn a_tool_is_sorted_by_name_whatever_the_case() {
        for name in ["Bash", "bash"] {
            assert_eq!(category(name), Category::Command, "{name}");
        }
        for name in [
            "Edit",
            "edit",
            "Write",
            "write",
            "MultiEdit",
            "multi_edit",
            "NotebookEdit",
        ] {
            assert_eq!(category(name), Category::Edit, "{name}");
        }
        for name in [
            "Read",
            "read",
            "Grep",
            "grep",
            "Glob",
            "list",
            "LS",
            "code_symbol",
        ] {
            assert_eq!(category(name), Category::Read, "{name}");
        }
        for name in ["WebSearch", "mcp__fs__read", "task", ""] {
            assert_eq!(category(name), Category::Other, "{name}");
        }
    }

    #[test]
    fn durations_read_as_seconds_minutes_and_hours() {
        let cases = [
            (0, ""),
            (12, "12s"),
            (59, "59s"),
            (64, "1m 04s"),
            (192, "3m 12s"),
            (3600, "1h 00m"),
            (3900, "1h 05m"),
        ];
        for (seconds, text) in cases {
            assert_eq!(
                format_duration(Duration::from_secs(seconds)),
                text,
                "{seconds}"
            );
        }
    }

    #[test]
    fn a_settled_turn_folds_into_commands_and_edits() {
        // Arrange
        let calls = [
            ("1", "Bash", Some("cargo test"), false),
            ("2", "Bash", Some("cargo fmt"), false),
            ("3", "Bash", Some("ls"), false),
            ("4", "Bash", Some("pwd"), false),
            ("5", "Edit", Some("a.rs"), false),
            ("6", "Edit", Some("b.rs"), false),
            ("7", "Read", Some("c.rs"), false),
        ];

        // Act
        let fold = run(&calls, &result(false), 192).unwrap();

        // Assert
        assert_eq!(
            fold.line,
            "Worked for 3m 12s: ran 4 commands and edited 2 files"
        );
        assert_eq!(fold.details.len(), 7);
        assert_eq!(fold.details[0], "Bash: cargo test");
    }

    #[test]
    fn only_the_two_most_important_categories_are_named() {
        let calls = [
            ("1", "Read", Some("a.rs"), false),
            ("2", "Edit", Some("b.rs"), false),
            ("3", "Read", Some("c.rs"), false),
        ];

        let fold = run(&calls, &result(false), 5).unwrap();

        assert_eq!(fold.line, "Worked for 5s: edited 1 file and read 2 items");
    }

    #[test]
    fn reads_alone_are_named_when_nothing_else_ran() {
        let fold = run(&[("1", "Grep", Some("TODO"), false)], &result(false), 2).unwrap();

        assert_eq!(fold.line, "Worked for 2s: read 1 item");
    }

    #[test]
    fn an_edit_of_the_same_file_twice_counts_once() {
        let calls = [
            ("1", "Edit", Some("a.rs"), false),
            ("2", "Edit", Some("a.rs"), false),
        ];

        let fold = run(&calls, &result(false), 4).unwrap();

        assert_eq!(fold.line, "Worked for 4s: edited 1 file");
    }

    #[test]
    fn failures_are_marked_next_to_their_category() {
        let calls = [
            ("1", "Bash", Some("cargo test"), true),
            ("2", "Bash", Some("ls"), false),
            ("3", "Edit", Some("a.rs"), false),
        ];

        let fold = run(&calls, &result(false), 9).unwrap();

        assert_eq!(
            fold.line,
            "Worked for 9s: ran 2 commands (1 failed) and edited 1 file"
        );
        assert_eq!(fold.details[0], "Bash: cargo test (failed)");
    }

    #[test]
    fn a_turn_with_no_counted_work_does_not_fold() {
        assert_eq!(run(&[], &result(false), 30), None);
        let other = [("1", "WebSearch", Some("rust"), false)];
        assert_eq!(run(&other, &result(false), 30), None);
    }

    #[test]
    fn a_turn_under_a_second_names_no_time() {
        let fold = run(&[("1", "Bash", None, false)], &result(false), 0).unwrap();

        assert_eq!(fold.line, "Worked: ran 1 command");
    }

    #[test]
    fn an_error_result_and_an_interrupt_do_not_say_worked() {
        let calls = [("1", "Bash", Some("ls"), false)];

        let failed = run(&calls, &result(true), 7).unwrap();
        let stopped = run(&calls, &interrupted(), 7).unwrap();

        assert_eq!(failed.line, "Failed after 7s: ran 1 command");
        assert_eq!(stopped.line, "Stopped after 7s: ran 1 command");
    }

    #[test]
    fn a_call_still_open_when_the_turn_stops_is_cut_short_and_never_finished() {
        // Arrange
        let t0 = Instant::now();
        let mut log = WorkLog::new();
        log.begin(t0);
        log.apply(
            &call("1", "Bash", Some("ls"), ToolCallStatus::Succeeded),
            t0,
        );
        log.apply(
            &call("2", "Bash", Some("sleep 9"), ToolCallStatus::Started),
            t0,
        );

        // Act
        let fold = log
            .apply(&interrupted(), t0 + Duration::from_secs(3))
            .unwrap();

        // Assert
        assert_eq!(fold.line, "Stopped after 3s: ran 1 command, 1 cut short");
        assert!(!fold.line.contains("finished"));
        assert_eq!(fold.details[1], "Bash: sleep 9 (cut short)");
    }

    #[test]
    fn the_live_line_names_the_running_tool_and_its_time() {
        // Arrange
        let t0 = Instant::now();
        let mut log = WorkLog::new();
        log.begin(t0);
        log.apply(
            &call("1", "Bash", Some("cargo test"), ToolCallStatus::Started),
            t0,
        );

        // Act
        let live = log.live(t0 + Duration::from_secs(12), false);

        // Assert
        assert_eq!(live.as_deref(), Some("running cargo test 12s"));
    }

    #[test]
    fn the_live_line_uses_a_verb_for_the_category_and_counts_the_others() {
        let t0 = Instant::now();
        let mut log = WorkLog::new();
        log.begin(t0);
        log.apply(
            &call("1", "Edit", Some("a.rs"), ToolCallStatus::Started),
            t0,
        );
        log.apply(
            &call("2", "Read", Some("b.rs"), ToolCallStatus::Started),
            t0,
        );

        let live = log.live(t0 + Duration::from_secs(1), false);

        assert_eq!(live.as_deref(), Some("reading b.rs 1s (+1 more)"));
    }

    #[test]
    fn a_tool_without_a_summary_is_named() {
        let t0 = Instant::now();
        let mut log = WorkLog::new();
        log.begin(t0);
        log.apply(
            &call("1", "mcp__fs__stat", None, ToolCallStatus::Started),
            t0,
        );

        let live = log.live(t0 + Duration::from_secs(20), false);

        assert_eq!(live.as_deref(), Some("running mcp__fs__stat 20s"));
    }

    #[test]
    fn a_finished_tool_leaves_the_live_line() {
        let t0 = Instant::now();
        let mut log = WorkLog::new();
        log.begin(t0);
        log.apply(&call("1", "Bash", Some("ls"), ToolCallStatus::Started), t0);
        log.apply(
            &call("1", "Bash", Some("ls"), ToolCallStatus::Succeeded),
            t0,
        );

        assert_eq!(log.live(t0, false), None);
    }

    #[test]
    fn a_pending_approval_shows_a_wait_and_never_a_running_tool() {
        let t0 = Instant::now();
        let mut log = WorkLog::new();
        log.begin(t0);
        log.apply(
            &call("1", "Bash", Some("rm x"), ToolCallStatus::Started),
            t0,
        );

        let live = log.live(t0 + Duration::from_secs(40), true);

        assert_eq!(live.as_deref(), Some("waiting for approval"));
    }

    #[test]
    fn a_second_turn_starts_with_nothing_from_the_first() {
        let t0 = Instant::now();
        let mut log = WorkLog::new();
        log.begin(t0);
        log.apply(&call("1", "Bash", Some("ls"), ToolCallStatus::Started), t0);
        log.apply(&result(false), t0);

        log.begin(t0);

        assert_eq!(log.live(t0, false), None);
        assert_eq!(log.apply(&result(false), t0), None);
    }

    #[test]
    fn a_replayed_turn_folds_without_a_begin_call() {
        let t0 = Instant::now();
        let mut log = WorkLog::new();
        log.apply(&call("1", "Bash", Some("ls"), ToolCallStatus::Started), t0);
        log.apply(
            &call("1", "Bash", Some("ls"), ToolCallStatus::Succeeded),
            t0,
        );

        let fold = log.apply(&result(false), t0).unwrap();

        assert_eq!(fold.line, "Worked: ran 1 command");
    }

    #[test]
    fn a_status_from_a_newer_peer_changes_nothing() {
        let t0 = Instant::now();
        let mut log = WorkLog::new();
        log.begin(t0);

        log.apply(&call("1", "Bash", None, ToolCallStatus::Unknown), t0);

        assert_eq!(log.live(t0, false), None);
    }
}
