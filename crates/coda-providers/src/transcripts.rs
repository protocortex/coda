// SPDX-License-Identifier: Apache-2.0

//! Token usage per day, model and project, read from the transcripts that Claude
//! Code and Codex keep on disk.
//!
//! The files are read only, and only the token counts, the model, the time and the
//! working folder of a line are looked at; text and tool output are never kept.
//! The formats are the vendors' private ones, so a line that does not fit is skipped
//! and counted, and nothing here fails on one. Days are UTC, from the line's own
//! timestamp.
//!
//! Claude Code writes the same reply to several lines (one per content block) and
//! copies earlier replies into the file of a resumed session, so a reply counts once,
//! by its message id and request id, with its largest output. Codex reports a running
//! total per session, so each report adds the step since the one before; a forked
//! session repeats earlier reports, which count once by time and total. Codex counts
//! cached tokens inside its input, so `input_tokens` here is the input that was not
//! cached, as it is for Claude Code.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use coda_protocol::types::TokenUsage;
use serde_json::Value;

/// A line longer than this is skipped, so one damaged file cannot use all the memory.
const MAX_LINE_BYTES: usize = 32 * 1024 * 1024;
/// Deepest folder level read below a root; Claude Code nests sub-agents two levels down.
const MAX_DEPTH: usize = 8;
const JSONL_SUFFIX: &str = ".jsonl";
const ZSTD_SUFFIX: &str = ".jsonl.zst";
const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
const CODEX_HOME_ENV: &str = "CODEX_HOME";
const HOME_ENV: &str = "HOME";
const CLAUDE_DEFAULT_DIR: &str = ".claude";
const CLAUDE_PROJECTS_DIR: &str = "projects";
const CODEX_DEFAULT_DIR: &str = ".codex";
const CODEX_SESSIONS_DIR: &str = "sessions";
/// The model Claude Code names on replies it made up itself, such as an error notice.
const SYNTHETIC_MODEL: &str = "<synthetic>";
const UNKNOWN: &str = "unknown";
const DAY_CHARS: usize = 10;

/// Where the transcripts live; a missing root reads as empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptRoots {
    /// The folder with Claude Code's project transcripts.
    pub claude: Option<PathBuf>,
    /// The folder with Codex's session transcripts.
    pub codex: Option<PathBuf>,
}

impl TranscriptRoots {
    /// The default places: `$CLAUDE_CONFIG_DIR/projects` or `~/.claude/projects`, and
    /// `$CODEX_HOME/sessions` or `~/.codex/sessions`.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    /// [`Self::from_env`] with the variables read through `lookup`. An empty value counts
    /// as unset, so it never turns a root into the current folder.
    #[must_use]
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Self {
        let value = |name: &str| lookup(name).filter(|value| !value.is_empty());
        let home = value(HOME_ENV).map(PathBuf::from);
        let pick = |variable: &str, default: &str, leaf: &str| {
            value(variable)
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(default)))
                .map(|base| base.join(leaf))
        };
        Self {
            claude: pick(
                CLAUDE_CONFIG_DIR_ENV,
                CLAUDE_DEFAULT_DIR,
                CLAUDE_PROJECTS_DIR,
            ),
            codex: pick(CODEX_HOME_ENV, CODEX_DEFAULT_DIR, CODEX_SESSIONS_DIR),
        }
    }
}

/// Which tool wrote a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// Claude Code.
    Claude,
    /// Codex.
    Codex,
}

impl Source {
    /// The name shown to the user.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// The tokens of one day, tool, model and project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRow {
    /// The UTC day, `YYYY-MM-DD`.
    pub day: String,
    /// The tool that wrote the transcript.
    pub source: Source,
    /// The model, as the transcript names it.
    pub model: String,
    /// The working folder, or the transcript folder's name when a line has none.
    pub project: String,
    /// The tokens, summed.
    pub usage: TokenUsage,
}

/// What reading the transcripts found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    /// One row per day, tool, model and project, oldest day first.
    pub rows: Vec<UsageRow>,
    /// Lines that were not JSON or were too long to read.
    pub skipped_lines: u64,
    /// Files that could not be opened or decoded.
    pub unreadable_files: u64,
}

type Key = (String, Source, String, String);

/// Sums tokens by day, tool, model and project.
#[derive(Default)]
struct Totals {
    sums: BTreeMap<Key, TokenUsage>,
    skipped_lines: u64,
    unreadable_files: u64,
}

impl Totals {
    fn add(&mut self, key: Key, usage: TokenUsage) {
        let sum = self.sums.entry(key).or_default();
        sum.input_tokens += usage.input_tokens;
        sum.output_tokens += usage.output_tokens;
        sum.cache_read_tokens += usage.cache_read_tokens;
        sum.cache_write_tokens += usage.cache_write_tokens;
    }
}

/// Reads every transcript under the roots and sums the tokens.
#[must_use]
pub fn read_history(roots: &TranscriptRoots) -> History {
    let mut totals = Totals::default();
    if let Some(root) = &roots.claude {
        read_claude(root, &mut totals);
    }
    if let Some(root) = &roots.codex {
        read_codex(root, &mut totals);
    }
    History {
        rows: totals
            .sums
            .into_iter()
            .map(|((day, source, model, project), usage)| UsageRow {
                day,
                source,
                model,
                project,
                usage,
            })
            .collect(),
        skipped_lines: totals.skipped_lines,
        unreadable_files: totals.unreadable_files,
    }
}

/// The transcript files below `root`, in name order. Links are not followed.
fn transcript_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() && depth < MAX_DEPTH {
                walk(&path, depth + 1, out);
            } else if kind.is_file() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(JSONL_SUFFIX) || name.ends_with(ZSTD_SUFFIX) {
                    out.push(path);
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(root, 0, &mut files);
    files.sort();
    files
}

/// Calls `each` with every line of the file, plain or zstd. Returns false when the
/// file could not be opened or decoded, after the lines that did read.
fn for_each_line(path: &Path, skipped: &mut u64, mut each: impl FnMut(&str)) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let compressed = path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(ZSTD_SUFFIX));
    if compressed {
        match ruzstd::decoding::StreamingDecoder::new(file) {
            Ok(decoder) => read_lines(BufReader::new(decoder), skipped, &mut each),
            Err(_) => false,
        }
    } else {
        read_lines(BufReader::new(file), skipped, &mut each)
    }
}

fn read_lines(mut reader: impl BufRead, skipped: &mut u64, each: &mut impl FnMut(&str)) -> bool {
    let mut line = Vec::new();
    loop {
        line.clear();
        let limit = u64::try_from(MAX_LINE_BYTES).unwrap_or(u64::MAX) + 1;
        match (&mut reader).take(limit).read_until(b'\n', &mut line) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
        if line.len() > MAX_LINE_BYTES && line.last() != Some(&b'\n') {
            *skipped += 1;
            if discard_line(&mut reader).is_err() {
                return false;
            }
            continue;
        }
        match std::str::from_utf8(&line) {
            Ok(text) => each(text.trim_end()),
            Err(_) => *skipped += 1,
        }
    }
}

/// Drops the rest of an over-long line.
fn discard_line(reader: &mut impl BufRead) -> std::io::Result<()> {
    loop {
        let (used, done) = {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                return Ok(());
            }
            match buffer.iter().position(|byte| *byte == b'\n') {
                Some(at) => (at + 1, true),
                None => (buffer.len(), false),
            }
        };
        reader.consume(used);
        if done {
            return Ok(());
        }
    }
}

/// The `YYYY-MM-DD` start of an ISO timestamp.
fn day_of(timestamp: &str) -> Option<String> {
    let day = timestamp.get(..DAY_CHARS)?;
    let shaped = day.bytes().enumerate().all(|(at, byte)| match at {
        4 | 7 => byte == b'-',
        _ => byte.is_ascii_digit(),
    });
    shaped.then(|| day.to_owned())
}

/// Whether a line is shaped like a whole JSON object, which a cut or damaged line is not.
fn is_object_text(line: &str) -> bool {
    line.starts_with('{') && line.ends_with('}')
}

fn parse(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line)
        .ok()
        .filter(Value::is_object)
}

struct ClaudeReply {
    key: Key,
    usage: TokenUsage,
}

fn read_claude(root: &Path, totals: &mut Totals) {
    let mut replies: HashMap<(String, String), ClaudeReply> = HashMap::new();
    let mut anonymous = 0_u64;
    let mut broken = 0_u64;
    for path in transcript_files(root) {
        let folder = path
            .strip_prefix(root)
            .ok()
            .and_then(|relative| relative.components().next())
            .map_or_else(
                || UNKNOWN.to_owned(),
                |part| part.as_os_str().to_string_lossy().into_owned(),
            );
        let readable = for_each_line(&path, &mut totals.skipped_lines, |line| {
            if line.is_empty() {
                return;
            }
            if !is_object_text(line) {
                broken += 1;
                return;
            }
            if !line.contains("\"usage\"") {
                return;
            }
            let Some(value) = parse(line) else {
                broken += 1;
                return;
            };
            let Some(reply) = claude_reply(&value, &folder) else {
                if carries_usage(&value) {
                    broken += 1;
                }
                return;
            };
            let id = value["message"]["id"].as_str();
            let request = value["requestId"].as_str();
            let identity = if let (Some(id), Some(request)) = (id, request) {
                (id.to_owned(), request.to_owned())
            } else {
                // A reply with no ids cannot be matched, so it counts as it stands.
                anonymous += 1;
                (String::new(), anonymous.to_string())
            };
            match replies.get_mut(&identity) {
                Some(seen) if seen.usage.output_tokens >= reply.usage.output_tokens => {}
                Some(seen) => *seen = reply,
                None => {
                    replies.insert(identity, reply);
                }
            }
        });
        if !readable {
            totals.unreadable_files += 1;
        }
    }
    totals.skipped_lines += broken;
    for reply in replies.into_values() {
        totals.add(reply.key, reply.usage);
    }
}

/// Whether a line is a reply that reports usage, whether or not its fields can be read.
fn carries_usage(value: &Value) -> bool {
    value["type"] == "assistant"
        && value["message"]["usage"].is_object()
        && value["message"]["model"] != SYNTHETIC_MODEL
}

fn claude_reply(value: &Value, folder: &str) -> Option<ClaudeReply> {
    if value["type"] != "assistant" {
        return None;
    }
    let message = &value["message"];
    let model = message["model"]
        .as_str()
        .filter(|model| *model != SYNTHETIC_MODEL)?;
    let day = day_of(value["timestamp"].as_str()?)?;
    let counts = &message["usage"];
    let count = |name: &str| match &counts[name] {
        Value::Null => Some(0),
        other => other.as_u64(),
    };
    let usage = TokenUsage {
        input_tokens: counts["input_tokens"].as_u64()?,
        output_tokens: counts["output_tokens"].as_u64()?,
        cache_read_tokens: count("cache_read_input_tokens")?,
        cache_write_tokens: count("cache_creation_input_tokens")?,
    };
    let project = value["cwd"].as_str().unwrap_or(folder).to_owned();
    Some(ClaudeReply {
        key: (day, Source::Claude, model.to_owned(), project),
        usage,
    })
}

fn read_codex(root: &Path, totals: &mut Totals) {
    let mut seen: HashSet<(String, [u64; 4])> = HashSet::new();
    let mut broken = 0_u64;
    for path in transcript_files(root) {
        let mut model = UNKNOWN.to_owned();
        let mut project = UNKNOWN.to_owned();
        let mut before = [0_u64; 4];
        let mut steps = Vec::new();
        let readable = for_each_line(&path, &mut totals.skipped_lines, |line| {
            if line.is_empty() {
                return;
            }
            if !is_object_text(line) {
                broken += 1;
                return;
            }
            let interesting = ["session_meta", "turn_context", "token_count"]
                .iter()
                .any(|marker| line.contains(marker));
            if !interesting {
                return;
            }
            let Some(value) = parse(line) else {
                return;
            };
            let payload = &value["payload"];
            match value["type"].as_str() {
                Some("session_meta") => {
                    if let Some(cwd) = payload["cwd"].as_str() {
                        cwd.clone_into(&mut project);
                    }
                }
                Some("turn_context") => {
                    if let Some(name) = payload["model"].as_str() {
                        name.clone_into(&mut model);
                    }
                    if let Some(cwd) = payload["cwd"].as_str() {
                        cwd.clone_into(&mut project);
                    }
                }
                Some("event_msg") if payload["type"] == "token_count" => {
                    if payload["info"].is_null() {
                        return;
                    }
                    let now = codex_total(&payload["info"]["total_token_usage"]);
                    let (Some(now), Some(stamp)) = (now, value["timestamp"].as_str()) else {
                        broken += 1;
                        return;
                    };
                    // A total that went down is a new counter, so its whole value is the step.
                    let base = if now[0] < before[0] { [0; 4] } else { before };
                    before = now;
                    let step = [
                        now[0].saturating_sub(base[0]),
                        now[1].saturating_sub(base[1]),
                        now[2].saturating_sub(base[2]),
                        now[3].saturating_sub(base[3]),
                    ];
                    if let Some(day) = day_of(stamp)
                        && seen.insert((stamp.to_owned(), now))
                    {
                        steps.push((day, model.clone(), project.clone(), step));
                    }
                }
                _ => {}
            }
        });
        if !readable {
            totals.unreadable_files += 1;
        }
        totals.skipped_lines += std::mem::take(&mut broken);
        for (day, model, project, [input, cached, written, output]) in steps {
            totals.add(
                (day, Source::Codex, model, project),
                TokenUsage {
                    input_tokens: input.saturating_sub(cached),
                    output_tokens: output,
                    cache_read_tokens: cached,
                    cache_write_tokens: written,
                },
            );
        }
    }
}

/// `[input, cached input, cache write, output]` of a Codex running total.
fn codex_total(total: &Value) -> Option<[u64; 4]> {
    let count = |name: &str| match &total[name] {
        Value::Null => Some(0),
        other => other.as_u64(),
    };
    Some([
        total["input_tokens"].as_u64()?,
        count("cached_input_tokens")?,
        count("cache_write_input_tokens")?,
        total["output_tokens"].as_u64()?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::io::Write;

    struct Rig {
        dir: tempfile::TempDir,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn roots(&self) -> TranscriptRoots {
            TranscriptRoots {
                claude: Some(self.dir.path().join("claude")),
                codex: Some(self.dir.path().join("codex")),
            }
        }

        fn write(&self, relative: &str, lines: &[String]) -> PathBuf {
            let path = self.dir.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, lines.join("\n") + "\n").unwrap();
            path
        }
    }

    fn reply(id: &str, request: &str, day: &str, model: &str, usage: &serde_json::Value) -> String {
        json!({
            "type": "assistant", "requestId": request, "cwd": "/work/app",
            "timestamp": format!("{day}T10:00:00.000Z"),
            "message": {"id": id, "model": model, "usage": usage},
        })
        .to_string()
    }

    fn usage(input: u64, output: u64, read: u64, write: u64) -> serde_json::Value {
        json!({"input_tokens": input, "output_tokens": output,
            "cache_read_input_tokens": read, "cache_creation_input_tokens": write})
    }

    fn tokens(input: u64, output: u64, read: u64, write: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: read,
            cache_write_tokens: write,
        }
    }

    #[test]
    fn claude_replies_are_summed_per_day_model_and_project() {
        // Arrange
        let rig = Rig::new();
        rig.write(
            "claude/p/a.jsonl",
            &[
                reply(
                    "m1",
                    "r1",
                    "2026-10-27",
                    "claude-haiku",
                    &usage(2, 10, 100, 50),
                ),
                reply(
                    "m2",
                    "r2",
                    "2026-10-27",
                    "claude-haiku",
                    &usage(3, 20, 200, 0),
                ),
                reply("m3", "r3", "2026-10-28", "claude-opus", &usage(1, 5, 0, 0)),
                json!({"type": "user", "message": {"content": "hello"}}).to_string(),
                json!({"type": "queue-operation"}).to_string(),
            ],
        );

        // Act
        let history = read_history(&rig.roots());

        // Assert
        let rows: Vec<_> = history
            .rows
            .iter()
            .map(|r| {
                (
                    r.day.as_str(),
                    r.source,
                    r.model.as_str(),
                    r.project.as_str(),
                    r.usage,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    "2026-10-27",
                    Source::Claude,
                    "claude-haiku",
                    "/work/app",
                    tokens(5, 30, 300, 50)
                ),
                (
                    "2026-10-28",
                    Source::Claude,
                    "claude-opus",
                    "/work/app",
                    tokens(1, 5, 0, 0)
                ),
            ]
        );
        assert_eq!(history.skipped_lines, 0);
    }

    #[test]
    fn a_reply_written_on_several_lines_or_copied_to_a_resumed_file_counts_once() {
        // Arrange
        let rig = Rig::new();
        rig.write(
            "claude/p/a.jsonl",
            &[
                reply(
                    "m1",
                    "r1",
                    "2026-10-27",
                    "claude-haiku",
                    &usage(2, 4, 100, 50),
                ),
                reply(
                    "m1",
                    "r1",
                    "2026-10-27",
                    "claude-haiku",
                    &usage(2, 40, 100, 50),
                ),
            ],
        );
        rig.write(
            "claude/p/b.jsonl",
            &[reply(
                "m1",
                "r1",
                "2026-10-27",
                "claude-haiku",
                &usage(2, 40, 100, 50),
            )],
        );

        // Act
        let history = read_history(&rig.roots());

        // Assert
        assert_eq!(history.rows.len(), 1);
        assert_eq!(history.rows[0].usage, tokens(2, 40, 100, 50));
    }

    #[test]
    fn lines_that_do_not_fit_are_skipped_and_only_broken_ones_are_counted() {
        // Arrange
        let rig = Rig::new();
        rig.write(
            "claude/p/a.jsonl",
            &[
                "{not json".to_owned(),
                reply("m1", "r1", "2026-10-27", "<synthetic>", &usage(9, 9, 9, 9)),
                reply("m2", "r2", "yesterday", "claude-haiku", &usage(9, 9, 9, 9)),
                json!({"type": "assistant", "message": {"id": "m3", "model": "x"}}).to_string(),
                json!({"type": "assistant", "timestamp": "2026-10-27T01:00:00Z",
                    "message": {"id": "m4", "model": "x", "usage": {"input_tokens": "many"}}})
                .to_string(),
                reply("m5", "r5", "2026-10-27", "claude-haiku", &usage(1, 1, 0, 0)),
            ],
        );

        // Act
        let history = read_history(&rig.roots());

        // Assert
        assert_eq!(history.skipped_lines, 3);
        assert_eq!(history.rows.len(), 1);
        assert_eq!(history.rows[0].usage, tokens(1, 1, 0, 0));
    }

    #[test]
    fn the_project_is_the_working_folder_else_the_transcript_folder() {
        // Arrange
        let rig = Rig::new();
        let no_cwd = json!({"type": "assistant", "requestId": "r", "timestamp": "2026-10-27T01:00:00Z",
            "message": {"id": "m", "model": "claude-haiku", "usage": usage(1, 1, 0, 0)}});
        rig.write("claude/-Users-me-app/a.jsonl", &[no_cwd.to_string()]);

        // Act
        let history = read_history(&rig.roots());

        // Assert
        assert_eq!(history.rows[0].project, "-Users-me-app");
    }

    #[test]
    fn transcripts_of_sub_agents_in_nested_folders_are_read() {
        // Arrange
        let rig = Rig::new();
        rig.write(
            "claude/p/session/subagents/agent-1.jsonl",
            &[reply(
                "m1",
                "r1",
                "2026-10-27",
                "claude-haiku",
                &usage(1, 2, 3, 4),
            )],
        );

        // Act
        let history = read_history(&rig.roots());

        // Assert
        assert_eq!(history.rows[0].usage, tokens(1, 2, 3, 4));
    }

    #[test]
    fn a_symbolic_link_is_not_followed() {
        // Arrange
        let rig = Rig::new();
        let outside = rig.write(
            "elsewhere/a.jsonl",
            &[reply(
                "m1",
                "r1",
                "2026-10-27",
                "claude-haiku",
                &usage(1, 1, 1, 1),
            )],
        );
        fs::create_dir_all(rig.dir.path().join("claude/p")).unwrap();
        std::os::unix::fs::symlink(&outside, rig.dir.path().join("claude/p/link.jsonl")).unwrap();
        std::os::unix::fs::symlink(
            rig.dir.path().join("elsewhere"),
            rig.dir.path().join("claude/linked-dir"),
        )
        .unwrap();

        // Act
        let history = read_history(&rig.roots());

        // Assert
        assert_eq!(history.rows, []);
    }

    fn codex_report(at: &str, total: (u64, u64, u64)) -> String {
        json!({"timestamp": at, "type": "event_msg", "payload": {"type": "token_count",
            "info": {"total_token_usage": {"input_tokens": total.0, "cached_input_tokens": total.1,
                "cache_write_input_tokens": 0, "output_tokens": total.2}}}})
        .to_string()
    }

    fn codex_lines() -> Vec<String> {
        vec![
            json!({"timestamp": "2026-11-03T08:00:00Z", "type": "session_meta",
                "payload": {"cwd": "/work/api"}})
            .to_string(),
            json!({"timestamp": "2026-11-03T08:00:01Z", "type": "turn_context",
                "payload": {"model": "gpt-6", "cwd": "/work/api"}})
            .to_string(),
            codex_report("2026-11-03T08:01:00Z", (1000, 600, 50)),
            json!({"timestamp": "2026-11-03T08:01:30Z", "type": "event_msg",
                "payload": {"type": "token_count", "info": null}})
            .to_string(),
            codex_report("2026-11-04T00:30:00Z", (1800, 1000, 90)),
        ]
    }

    #[test]
    fn codex_totals_become_steps_on_the_day_of_each_report_without_cached_input_in_input() {
        // Arrange
        let rig = Rig::new();
        rig.write("codex/2026/11/03/rollout-a.jsonl", &codex_lines());

        // Act
        let history = read_history(&rig.roots());

        // Assert
        let rows: Vec<_> = history
            .rows
            .iter()
            .map(|r| {
                (
                    r.day.as_str(),
                    r.source,
                    r.model.as_str(),
                    r.project.as_str(),
                    r.usage,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    "2026-11-03",
                    Source::Codex,
                    "gpt-6",
                    "/work/api",
                    tokens(400, 50, 600, 0)
                ),
                (
                    "2026-11-04",
                    Source::Codex,
                    "gpt-6",
                    "/work/api",
                    tokens(400, 40, 400, 0)
                ),
            ]
        );
    }

    #[test]
    fn a_compressed_codex_transcript_is_read_like_a_plain_one() {
        // Arrange
        let rig = Rig::new();
        let text = codex_lines().join("\n") + "\n";
        let packed = ruzstd::encoding::compress_to_vec(
            text.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let path = rig.dir.path().join("codex/2026/11/03/rollout-a.jsonl.zst");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::File::create(&path).unwrap().write_all(&packed).unwrap();

        // Act
        let history = read_history(&rig.roots());

        // Assert
        assert_eq!(history.rows.len(), 2);
        assert_eq!(history.unreadable_files, 0);
    }

    #[test]
    fn a_forked_codex_session_does_not_count_the_reports_it_copied() {
        // Arrange
        let rig = Rig::new();
        rig.write("codex/2026/11/03/rollout-a.jsonl", &codex_lines());
        let mut forked = codex_lines();
        forked.push(codex_report("2026-11-04T01:00:00Z", (2000, 1000, 100)));
        rig.write("codex/2026/11/04/rollout-a_b.jsonl", &forked);

        // Act
        let history = read_history(&rig.roots());

        // Assert
        let output: u64 = history.rows.iter().map(|r| r.usage.output_tokens).sum();
        assert_eq!(output, 100);
    }

    #[test]
    fn a_file_that_cannot_be_decoded_is_counted_and_the_rest_still_reads() {
        // Arrange
        let rig = Rig::new();
        let bad = rig
            .dir
            .path()
            .join("codex/2026/11/03/rollout-bad.jsonl.zst");
        fs::create_dir_all(bad.parent().unwrap()).unwrap();
        fs::write(&bad, b"not zstd at all").unwrap();
        rig.write("codex/2026/11/03/rollout-a.jsonl", &codex_lines());

        // Act
        let history = read_history(&rig.roots());

        // Assert
        assert_eq!(history.unreadable_files, 1);
        assert_eq!(history.rows.len(), 2);
    }

    #[test]
    fn a_codex_report_that_cannot_be_read_is_counted_and_an_empty_one_is_not() {
        // Arrange
        let rig = Rig::new();
        let mut lines = codex_lines();
        lines.push(
            json!({"timestamp": "2026-11-04T02:00:00Z", "type": "event_msg",
                "payload": {"type": "token_count", "info": {"total_token_usage": {"input_tokens": "x"}}}})
            .to_string(),
        );
        rig.write("codex/2026/11/03/rollout-a.jsonl", &lines);

        // Act
        let history = read_history(&rig.roots());

        // Assert
        assert_eq!(history.skipped_lines, 1);
    }

    #[test]
    fn the_largest_output_of_a_repeated_reply_wins_whatever_the_order() {
        // Arrange
        let rig = Rig::new();
        rig.write(
            "claude/p/a.jsonl",
            &[
                reply(
                    "m1",
                    "r1",
                    "2026-10-27",
                    "claude-haiku",
                    &usage(2, 40, 0, 0),
                ),
                reply("m1", "r1", "2026-10-27", "claude-haiku", &usage(2, 4, 0, 0)),
            ],
        );

        // Act
        let history = read_history(&rig.roots());

        // Assert
        assert_eq!(history.rows[0].usage.output_tokens, 40);
    }

    #[test]
    fn replies_without_ids_all_count() {
        // Arrange
        let rig = Rig::new();
        let bare = json!({"type": "assistant", "timestamp": "2026-10-27T01:00:00Z",
            "message": {"model": "claude-haiku", "usage": usage(1, 1, 0, 0)}})
        .to_string();
        rig.write("claude/p/a.jsonl", &[bare.clone(), bare]);

        // Act
        let history = read_history(&rig.roots());

        // Assert
        assert_eq!(history.rows[0].usage, tokens(2, 2, 0, 0));
    }

    #[test]
    fn a_codex_total_that_goes_down_starts_a_new_counter() {
        // Arrange
        let rig = Rig::new();
        rig.write(
            "codex/2026/11/03/rollout-a.jsonl",
            &[
                codex_report("2026-11-03T08:00:00Z", (1000, 0, 100)),
                codex_report("2026-11-03T09:00:00Z", (300, 0, 30)),
            ],
        );

        // Act
        let history = read_history(&rig.roots());

        // Assert
        assert_eq!(history.rows[0].usage, tokens(1300, 130, 0, 0));
    }

    #[test]
    fn an_empty_variable_counts_as_unset_and_a_set_one_wins_over_home() {
        // Arrange
        let lookup = |set: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                set.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };

        // Act
        let empty = TranscriptRoots::from_lookup(lookup(&[
            ("HOME", "/home/me"),
            ("CLAUDE_CONFIG_DIR", ""),
            ("CODEX_HOME", ""),
        ]));
        let set = TranscriptRoots::from_lookup(lookup(&[
            ("HOME", "/home/me"),
            ("CODEX_HOME", "/data/codex"),
        ]));
        let none = TranscriptRoots::from_lookup(lookup(&[]));

        // Assert
        assert_eq!(
            empty.claude,
            Some(PathBuf::from("/home/me/.claude/projects"))
        );
        assert_eq!(empty.codex, Some(PathBuf::from("/home/me/.codex/sessions")));
        assert_eq!(set.codex, Some(PathBuf::from("/data/codex/sessions")));
        assert_eq!(set.claude, Some(PathBuf::from("/home/me/.claude/projects")));
        assert_eq!(none, TranscriptRoots::default());
    }

    #[test]
    fn missing_roots_read_as_empty() {
        // Arrange
        let rig = Rig::new();

        // Act
        let history = read_history(&rig.roots());
        let none = read_history(&TranscriptRoots::default());

        // Assert
        assert_eq!(history, History::default());
        assert_eq!(none, History::default());
    }
}
