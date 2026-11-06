// SPDX-License-Identifier: Apache-2.0

//! The `multi_edit` tool: several edits to one or more files that all apply or none do,
//! or the replacement of one indexed symbol.

use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{Stat, fstat};
use serde::Deserialize;
use serde_json::Value;

use crate::code::{CodeIndex, Freshness};
use crate::edit::{
    Original, Staged, ensure_unchanged, locate_unique, replace_file, stage_file, to_crlf,
};
use crate::sandbox::{Sandbox, SandboxDir};
use crate::{Tool, ToolError, ToolOutput, parse_args};

pub const NAME: &str = "multi_edit";

/// Most edits one call may hold.
const MAX_EDITS: usize = 100;
/// Match lines named in a message before it says no more.
const LINES_LISTED: usize = 10;
/// Longest line quoted back in a message.
const QUOTED_LINE_CHARS: usize = 200;
/// Characters a line must share with the missing text to be called near it.
const NEAREST_MIN_SHARED: usize = 4;

type Hook = Arc<dyn Fn() + Send + Sync>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    edits: Option<Vec<EditItem>>,
    symbol: Option<SymbolItem>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditItem {
    path: String,
    old_string: String,
    new_string: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolItem {
    path: String,
    name: String,
    new_text: String,
}

/// Applies a list of edits, or one symbol replacement, all or nothing.
///
/// Every edit is matched against the files as they were when the call began and
/// must find its text once, so no edit depends on another. Each changed file is
/// written to a temp file next to it first; only when all of those are ready are
/// they renamed into place, and a rename that fails puts the earlier ones back.
pub struct MultiEditTool {
    sandbox: Sandbox,
    max_file_bytes: usize,
    index: Arc<dyn CodeIndex>,
    before_stage: Option<Hook>,
    before_commit: Option<Hook>,
    before_restore: Option<Hook>,
}

impl MultiEditTool {
    /// Creates the tool, refusing files larger than `max_file_bytes`.
    #[must_use]
    pub fn new(sandbox: Sandbox, max_file_bytes: usize, index: Arc<dyn CodeIndex>) -> Self {
        Self {
            sandbox,
            max_file_bytes,
            index,
            before_stage: None,
            before_commit: None,
            before_restore: None,
        }
    }

    /// Runs `hook` after the edits are planned and before any file is written,
    /// so tests can change a file at the exact moment a race would.
    #[cfg(test)]
    #[must_use]
    fn with_before_stage(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.before_stage = Some(Arc::new(hook));
        self
    }

    /// Runs `hook` after every temp file is ready and before the first rename.
    #[cfg(test)]
    #[must_use]
    fn with_before_commit(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.before_commit = Some(Arc::new(hook));
        self
    }

    /// Runs `hook` after a rename failed and before the earlier files are put back.
    #[cfg(test)]
    #[must_use]
    fn with_before_restore(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.before_restore = Some(Arc::new(hook));
        self
    }

    /// The index of the target for `path`, opening and reading the file the
    /// first time. A file reached by two spellings is one target.
    fn target(&self, targets: &mut Vec<Target>, path: &str) -> Result<usize, ToolError> {
        let io_err = |source| ToolError::Io {
            path: path.to_owned(),
            source,
        };
        let (dir, name) = self.sandbox.open_parent(Path::new(path))?;
        let mut file = dir.open_file(&name)?;
        let stat = fstat(&file).map_err(|e| io_err(e.into()))?;
        if let Some(at) = targets
            .iter()
            .position(|t| t.stat.st_dev == stat.st_dev && t.stat.st_ino == stat.st_ino)
        {
            return Ok(at);
        }
        let snapshot = file.metadata().map_err(io_err)?;
        let limit = u64::try_from(self.max_file_bytes).unwrap_or(u64::MAX);
        let mut bytes = Vec::new();
        (&mut file)
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(io_err)?;
        if bytes.len() > self.max_file_bytes {
            return Err(ToolError::TooLarge {
                path: path.to_owned(),
                limit: self.max_file_bytes,
            });
        }
        let text = String::from_utf8(bytes).map_err(|_| ToolError::NotUtf8 {
            path: path.to_owned(),
        })?;
        targets.push(Target {
            path: path.to_owned(),
            dir,
            name,
            file,
            stat,
            snapshot,
            text,
            changes: Vec::new(),
        });
        Ok(targets.len() - 1)
    }

    fn plan_edits(&self, items: &[EditItem], targets: &mut Vec<Target>) -> Result<(), ToolError> {
        let total = items.len();
        for (index, item) in items.iter().enumerate() {
            let number = index + 1;
            let reject = |reason| rejected(number, total, &item.path, reason);
            let at = self.target(targets, &item.path)?;
            let target = &mut targets[at];
            let found = locate_unique(&item.path, &target.text, &item.old_string, &item.new_string)
                .map_err(|err| reject(describe(&err, &target.text, &item.old_string)))?;
            target
                .add(Change {
                    start: found.start,
                    end: found.end,
                    replacement: found.new,
                    number,
                })
                .map_err(reject)?;
        }
        Ok(())
    }

    /// Plans the replacement of the lines the index holds for one symbol.
    fn plan_symbol(&self, item: &SymbolItem, targets: &mut Vec<Target>) -> Result<(), ToolError> {
        let reject = |reason| rejected(1, 1, &item.path, reason);
        let at = self.target(targets, &item.path)?;
        let target = &mut targets[at];
        // A file changed by something else leaves the index pointing at the
        // wrong lines, and a range that is only slightly off would cut a
        // definition in half.
        if !matches!(
            self.index.freshness().map_err(ToolError::Index)?,
            Freshness::Fresh { .. }
        ) {
            return Err(reject(
                "the index is out of date; read the file and use edits".to_owned(),
            ));
        }
        let indexed = self
            .index
            .outline(&index_path(&item.path))
            .map_err(ToolError::Index)?;
        let named: Vec<_> = indexed.iter().filter(|s| s.name == item.name).collect();
        let symbol = match named.as_slice() {
            [] => {
                return Err(reject(format!(
                    "no symbol named `{}` is indexed in this file",
                    item.name
                )));
            }
            [one] => *one,
            many => {
                let lines: Vec<usize> = many.iter().map(|s| s.line as usize).collect();
                return Err(reject(format!(
                    "{} symbols are named `{}` (lines {}); use edits to change one of them",
                    many.len(),
                    item.name,
                    list_lines(&lines)
                )));
            }
        };
        let (line, end_line) = (symbol.line as usize, symbol.end_line as usize);
        if end_line < line || line == 0 {
            return Err(reject(format!(
                "the index has no line range for `{}`; use edits instead",
                item.name
            )));
        }
        let starts = line_starts(&target.text);
        let out_of_date = || {
            reject(format!(
                "the index is out of date for `{}`: lines {line} to {end_line} no longer hold it; \
                 read the file and use edits",
                item.name
            ))
        };
        if end_line > starts.len() {
            return Err(out_of_date());
        }
        let start = starts[line - 1];
        let end = starts.get(end_line).copied().unwrap_or(target.text.len());
        let old = &target.text[start..end];
        if !old.contains(&item.name) {
            return Err(out_of_date());
        }
        let mut replacement = if old.contains("\r\n") {
            to_crlf(&item.new_text)
        } else {
            item.new_text.clone()
        };
        let line_ending = if old.contains("\r\n") { "\r\n" } else { "\n" };
        if old.ends_with('\n') && !replacement.is_empty() && !replacement.ends_with('\n') {
            replacement.push_str(line_ending);
        }
        target
            .add(Change {
                start,
                end,
                replacement,
                number: 1,
            })
            .map_err(reject)
    }

    /// Writes every target, or none of them.
    fn apply(&self, targets: &[Target]) -> Result<(), ToolError> {
        let contents: Vec<String> = targets.iter().map(Target::updated).collect();
        if let Some(hook) = &self.before_stage {
            hook();
        }
        let mut staged = Vec::new();
        for (target, contents) in targets.iter().zip(&contents) {
            let original = Original {
                file: &target.file,
                stat: &target.stat,
                snapshot: &target.snapshot,
            };
            match stage_file(
                &target.dir,
                &target.name,
                &original,
                contents.as_bytes(),
                &target.path,
            ) {
                Ok(next) => staged.push(next),
                Err(err) => {
                    discard_all(staged, targets);
                    return Err(err);
                }
            }
        }
        if let Some(hook) = &self.before_commit {
            hook();
        }
        // Staging fsyncs every file, so the check made while staging is old by now.
        for target in targets {
            if let Err(err) =
                ensure_unchanged(&target.dir, &target.name, &target.snapshot, &target.path)
            {
                discard_all(staged, targets);
                return Err(err);
            }
        }
        let mut waiting = staged.into_iter();
        let mut done = 0;
        while let Some(next) = waiting.next() {
            let target = &targets[done];
            if let Err(err) = next.commit(&target.dir, &target.name) {
                for (rest, target) in waiting.zip(&targets[done + 1..]) {
                    rest.discard(&target.dir);
                }
                if let Some(hook) = &self.before_restore {
                    hook();
                }
                let left: Vec<&str> = targets[..done]
                    .iter()
                    .filter(|target| !target.restore())
                    .map(|target| target.path.as_str())
                    .collect();
                if left.is_empty() {
                    return Err(err);
                }
                return Err(ToolError::PartlyApplied {
                    cause: err.to_string(),
                    paths: left.join(", "),
                });
            }
            done += 1;
        }
        Ok(())
    }
}

impl Tool for MultiEditTool {
    fn name(&self) -> &'static str {
        NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let args: Args = parse_args(NAME, input)?;
        let invalid = |reason: &str| ToolError::InvalidInput {
            tool: NAME,
            reason: reason.to_owned(),
        };
        let mut targets = Vec::new();
        let summary = match (args.edits, args.symbol) {
            (Some(items), None) => {
                if items.is_empty() || items.len() > MAX_EDITS {
                    return Err(invalid(&format!("edits must hold 1 to {MAX_EDITS} edits")));
                }
                if items.iter().any(|item| item.old_string.is_empty()) {
                    return Err(invalid("old_string must not be empty"));
                }
                self.plan_edits(&items, &mut targets)?;
                format!(
                    "applied {} to {}",
                    plural(items.len(), "edit"),
                    plural(targets.len(), "file")
                )
            }
            (None, Some(item)) => {
                self.plan_symbol(&item, &mut targets)?;
                let change = &targets[0].changes[0];
                let first = line_of(&targets[0].text, change.start);
                let last = line_of(&targets[0].text, change.end.saturating_sub(1));
                format!(
                    "replaced {} in {} (lines {first} to {last})",
                    item.name, item.path
                )
            }
            _ => {
                return Err(invalid(
                    "give either edits or symbol, not both and not neither",
                ));
            }
        };
        self.apply(&targets)?;
        Ok(ToolOutput {
            text: summary,
            truncated: false,
        })
    }
}

/// A file read for the call and the changes planned for it.
struct Target {
    path: String,
    dir: SandboxDir,
    name: OsString,
    file: File,
    stat: Stat,
    snapshot: Metadata,
    text: String,
    changes: Vec<Change>,
}

struct Change {
    start: usize,
    end: usize,
    replacement: String,
    /// The one-based position of the edit in the call.
    number: usize,
}

impl Target {
    /// Plans `change`, unless it touches text another edit already changes.
    fn add(&mut self, change: Change) -> Result<(), String> {
        if let Some(other) = self
            .changes
            .iter()
            .find(|other| other.start < change.end && change.start < other.end)
        {
            return Err(format!(
                "it overlaps edit {}, which changes the same text; merge them into one edit",
                other.number
            ));
        }
        self.changes.push(change);
        Ok(())
    }

    /// The text with every change applied.
    fn updated(&self) -> String {
        let mut ordered: Vec<&Change> = self.changes.iter().collect();
        ordered.sort_by_key(|change| std::cmp::Reverse(change.start));
        let mut text = self.text.clone();
        for change in ordered {
            text.replace_range(change.start..change.end, &change.replacement);
        }
        text
    }

    /// Puts the text read at the start of the call back; best effort, because
    /// the failure that led here is the one worth reporting.
    fn restore(&self) -> bool {
        let Ok(file) = self.dir.open_file(&self.name) else {
            return false;
        };
        let (Ok(stat), Ok(snapshot)) = (fstat(&file), file.metadata()) else {
            return false;
        };
        let original = Original {
            file: &file,
            stat: &stat,
            snapshot: &snapshot,
        };
        replace_file(
            &self.dir,
            &self.name,
            &original,
            self.text.as_bytes(),
            &self.path,
        )
        .is_ok()
    }
}

fn rejected(number: usize, total: usize, path: &str, reason: String) -> ToolError {
    ToolError::MultiEdit {
        number,
        total,
        path: path.to_owned(),
        reason,
    }
}

fn discard_all(staged: Vec<Staged>, targets: &[Target]) {
    for (next, target) in staged.into_iter().zip(targets) {
        next.discard(&target.dir);
    }
}

/// Why `err` stopped an edit, with the lines that help the model fix it.
fn describe(err: &ToolError, text: &str, old: &str) -> String {
    match err {
        ToolError::NoMatch { .. } => match nearest_line(text, old) {
            Some((number, line)) => {
                format!("old_string was not found; the nearest line is line {number}: {line}")
            }
            None => "old_string was not found".to_owned(),
        },
        ToolError::NotUnique { count, .. } => {
            let lines: Vec<usize> = text
                .match_indices(old)
                .map(|(at, _)| line_of(text, at))
                .collect();
            let at = if lines.is_empty() {
                String::new()
            } else {
                format!(" (lines {})", list_lines(&lines))
            };
            format!(
                "old_string matches {count} places{at}; add surrounding context to make it unique"
            )
        }
        other => other.to_string(),
    }
}

/// The line that shares the longest start with the first line of `old`.
fn nearest_line(text: &str, old: &str) -> Option<(usize, String)> {
    let wanted = old.lines().map(str::trim).find(|line| !line.is_empty())?;
    let (shared, number, line) = text
        .lines()
        .enumerate()
        .map(|(at, line)| (shared_start(wanted, line.trim()), at + 1, line.trim()))
        .max_by_key(|(shared, number, _)| (*shared, std::cmp::Reverse(*number)))?;
    (shared >= NEAREST_MIN_SHARED).then(|| {
        let quoted: String = line.chars().take(QUOTED_LINE_CHARS).collect();
        (number, quoted)
    })
}

fn shared_start(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

fn list_lines(lines: &[usize]) -> String {
    let mut shown: Vec<String> = lines
        .iter()
        .take(LINES_LISTED)
        .map(usize::to_string)
        .collect();
    if lines.len() > LINES_LISTED {
        shown.push("...".to_owned());
    }
    shown.join(", ")
}

/// The one-based line holding byte `offset`.
fn line_of(text: &str, offset: usize) -> usize {
    1 + text
        .bytes()
        .take(offset)
        .filter(|byte| *byte == b'\n')
        .count()
}

/// The byte offset where each line starts.
fn line_starts(text: &str) -> Vec<usize> {
    let mut at = 0;
    text.split_inclusive('\n')
        .map(|line| {
            let start = at;
            at += line.len();
            start
        })
        .collect()
}

/// `path` as the index spells it: without `.` components.
fn index_path(path: &str) -> String {
    Path::new(path)
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned()
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{count} {word}")
    } else {
        format!("{count} {word}s")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::code::Freshness;
    use crate::sandbox::SandboxError;
    use coda_protocol::readers::{IndexReader, ReaderError, SearchHit, SymbolLocation};
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    const LIMIT: usize = 1024;

    struct FakeIndex(Vec<SymbolLocation>, Freshness);

    impl IndexReader for FakeIndex {
        fn symbols(&self, query: &str) -> Result<Vec<SymbolLocation>, ReaderError> {
            Ok(self
                .0
                .iter()
                .filter(|s| s.name.starts_with(query))
                .cloned()
                .collect())
        }

        fn outline(&self, path: &str) -> Result<Vec<SymbolLocation>, ReaderError> {
            Ok(self.0.iter().filter(|s| s.path == path).cloned().collect())
        }

        fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchHit>, ReaderError> {
            Ok(Vec::new())
        }
    }

    impl CodeIndex for FakeIndex {
        fn freshness(&self) -> Result<Freshness, ReaderError> {
            Ok(self.1)
        }
    }

    fn symbol(name: &str, path: &str, line: u32, end_line: u32) -> SymbolLocation {
        SymbolLocation {
            name: name.to_owned(),
            kind: "fn".to_owned(),
            path: path.to_owned(),
            line,
            end_line,
        }
    }

    fn rig(files: &[(&str, &str)], symbols: Vec<SymbolLocation>) -> (TempDir, MultiEditTool) {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            fs::write(dir.path().join(name), body).unwrap();
        }
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let fresh = Freshness::Fresh {
            files: 1,
            symbols: 1,
        };
        let tool = MultiEditTool::new(sandbox, LIMIT, Arc::new(FakeIndex(symbols, fresh)));
        (dir, tool)
    }

    fn edit(path: &str, old: &str, new: &str) -> Value {
        json!({"path": path, "old_string": old, "new_string": new})
    }

    fn edits(items: &[Value]) -> Value {
        json!({"edits": items})
    }

    fn read(dir: &TempDir, name: &str) -> String {
        fs::read_to_string(dir.path().join(name)).unwrap()
    }

    fn rejected(err: &ToolError) -> (usize, &str) {
        match err {
            ToolError::MultiEdit { number, reason, .. } => (*number, reason),
            other => panic!("not a rejected edit: {other}"),
        }
    }

    #[test]
    fn edits_to_two_files_write_both() {
        // Arrange
        let (dir, tool) = rig(&[("a.txt", "one two"), ("b.txt", "three four")], vec![]);

        // Act
        let out = tool
            .call(&edits(&[
                edit("a.txt", "two", "2"),
                edit("b.txt", "three", "3"),
            ]))
            .unwrap();

        // Assert
        assert_eq!(read(&dir, "a.txt"), "one 2");
        assert_eq!(read(&dir, "b.txt"), "3 four");
        assert_eq!(out.text, "applied 2 edits to 2 files");
    }

    #[test]
    fn several_edits_to_one_file_all_apply() {
        // Arrange
        let (dir, tool) = rig(&[("a.txt", "alpha beta gamma")], vec![]);

        // Act
        let out = tool
            .call(&edits(&[
                edit("a.txt", "gamma", "G"),
                edit("a.txt", "alpha", "A"),
            ]))
            .unwrap();

        // Assert
        assert_eq!(read(&dir, "a.txt"), "A beta G");
        assert_eq!(out.text, "applied 2 edits to 1 file");
    }

    #[test]
    fn one_bad_edit_changes_no_file() {
        // Arrange
        let (dir, tool) = rig(&[("a.txt", "one two"), ("b.txt", "three four")], vec![]);

        // Act
        let err = tool
            .call(&edits(&[
                edit("a.txt", "two", "2"),
                edit("b.txt", "missing", "x"),
            ]))
            .unwrap_err();

        // Assert
        assert_eq!(rejected(&err).0, 2);
        assert_eq!(read(&dir, "a.txt"), "one two");
        assert_eq!(read(&dir, "b.txt"), "three four");
    }

    #[test]
    fn the_first_failing_edit_is_the_one_named() {
        // Arrange
        let (_dir, tool) = rig(&[("a.txt", "one two")], vec![]);

        // Act
        let err = tool
            .call(&edits(&[
                edit("a.txt", "one", "1"),
                edit("a.txt", "nope", "x"),
                edit("a.txt", "also nope", "y"),
            ]))
            .unwrap_err();

        // Assert
        assert_eq!(rejected(&err).0, 2);
    }

    #[test]
    fn a_missing_text_reports_the_nearest_line() {
        // Arrange
        let (_dir, tool) = rig(&[("a.txt", "fn alpha() {}\nfn beta(x: u8) {}\n")], vec![]);

        // Act
        let err = tool
            .call(&edits(&[edit(
                "a.txt",
                "fn beta(y: u8) {}",
                "fn beta() {}",
            )]))
            .unwrap_err();

        // Assert
        let (_, reason) = rejected(&err);
        assert!(reason.contains("not found"), "{reason}");
        assert!(reason.contains("line 2: fn beta(x: u8) {}"), "{reason}");
    }

    #[test]
    fn an_ambiguous_text_reports_its_lines() {
        // Arrange
        let (_dir, tool) = rig(&[("a.txt", "x\nsame\ny\nsame\n")], vec![]);

        // Act
        let err = tool
            .call(&edits(&[edit("a.txt", "same", "other")]))
            .unwrap_err();

        // Assert
        let (number, reason) = rejected(&err);
        assert_eq!(number, 1);
        assert!(reason.contains("2 places"), "{reason}");
        assert!(reason.contains("lines 2, 4"), "{reason}");
    }

    #[test]
    fn an_edit_that_depends_on_an_earlier_edit_is_reported() {
        // Arrange
        let (dir, tool) = rig(&[("a.txt", "one two")], vec![]);

        // Act
        let err = tool
            .call(&edits(&[
                edit("a.txt", "one", "uno"),
                edit("a.txt", "uno two", "dos"),
            ]))
            .unwrap_err();

        // Assert
        assert_eq!(rejected(&err).0, 2);
        assert_eq!(read(&dir, "a.txt"), "one two");
    }

    #[test]
    fn overlapping_edits_are_reported() {
        // Arrange
        let (dir, tool) = rig(&[("a.txt", "one two three")], vec![]);

        // Act
        let err = tool
            .call(&edits(&[
                edit("a.txt", "one two", "x"),
                edit("a.txt", "two three", "y"),
            ]))
            .unwrap_err();

        // Assert
        let (number, reason) = rejected(&err);
        assert_eq!(number, 2);
        assert!(reason.contains("overlaps edit 1"), "{reason}");
        assert_eq!(read(&dir, "a.txt"), "one two three");
    }

    #[test]
    fn one_file_spelled_two_ways_keeps_both_edits() {
        // Arrange
        let (dir, tool) = rig(&[("a.txt", "one two")], vec![]);

        // Act
        tool.call(&edits(&[
            edit("a.txt", "one", "1"),
            edit("./a.txt", "two", "2"),
        ]))
        .unwrap();

        // Assert
        assert_eq!(read(&dir, "a.txt"), "1 2");
    }

    #[test]
    fn a_path_outside_the_workspace_stops_the_whole_call() {
        // Arrange
        let (dir, tool) = rig(&[("a.txt", "one two")], vec![]);

        // Act
        let err = tool
            .call(&edits(&[
                edit("a.txt", "two", "2"),
                edit("../escape.txt", "x", "y"),
            ]))
            .unwrap_err();

        // Assert
        assert!(matches!(
            err,
            ToolError::Sandbox(SandboxError::ParentComponent(_))
        ));
        assert_eq!(read(&dir, "a.txt"), "one two");
    }

    #[test]
    fn a_file_that_changes_before_the_write_stops_the_whole_call() {
        // Arrange
        let (dir, tool) = rig(&[("a.txt", "one two"), ("b.txt", "three four")], vec![]);
        let b = dir.path().join("b.txt");
        let tool = tool.with_before_stage(move || fs::write(&b, "changed elsewhere").unwrap());

        // Act
        let err = tool
            .call(&edits(&[
                edit("a.txt", "two", "2"),
                edit("b.txt", "three", "3"),
            ]))
            .unwrap_err();

        // Assert
        assert!(matches!(err, ToolError::Conflict { .. }), "{err}");
        assert_eq!(read(&dir, "a.txt"), "one two");
        assert_eq!(read(&dir, "b.txt"), "changed elsewhere");
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 2, "temp files were left behind");
    }

    #[test]
    fn a_failed_rename_puts_the_earlier_files_back() {
        // Arrange
        let (dir, tool) = rig(&[("a.txt", "one two"), ("b.txt", "three four")], vec![]);
        let root = dir.path().to_path_buf();
        let tool = tool.with_before_commit(move || {
            for entry in fs::read_dir(&root).unwrap() {
                let path = entry.unwrap().path();
                if path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".b.txt")
                {
                    fs::remove_file(path).unwrap();
                }
            }
        });

        // Act
        let result = tool.call(&edits(&[
            edit("a.txt", "two", "2"),
            edit("b.txt", "three", "3"),
        ]));

        // Assert
        assert!(result.is_err());
        assert_eq!(read(&dir, "a.txt"), "one two");
        assert_eq!(read(&dir, "b.txt"), "three four");
    }

    const SOURCE: &str = "fn keep() {}\n\nfn target() {\n    old();\n}\n\nfn after() {}\n";

    #[test]
    fn a_symbol_replaces_exactly_its_indexed_lines() {
        // Arrange
        let (dir, tool) = rig(
            &[("a.rs", SOURCE)],
            vec![
                symbol("keep", "a.rs", 1, 1),
                symbol("target", "a.rs", 3, 5),
                symbol("after", "a.rs", 7, 7),
            ],
        );

        // Act
        let out = tool
            .call(&json!({"symbol": {
                "path": "a.rs", "name": "target", "new_text": "fn target() {\n    new();\n}",
            }}))
            .unwrap();

        // Assert
        assert_eq!(
            read(&dir, "a.rs"),
            "fn keep() {}\n\nfn target() {\n    new();\n}\n\nfn after() {}\n"
        );
        assert_eq!(out.text, "replaced target in a.rs (lines 3 to 5)");
    }

    #[test]
    fn an_ambiguous_symbol_name_is_refused() {
        // Arrange
        let source = "fn run() {}\nfn run() {}\n";
        let (dir, tool) = rig(
            &[("a.rs", source)],
            vec![symbol("run", "a.rs", 1, 1), symbol("run", "a.rs", 2, 2)],
        );

        // Act
        let err = tool
            .call(&json!({"symbol": {"path": "a.rs", "name": "run", "new_text": "x"}}))
            .unwrap_err();

        // Assert
        let (_, reason) = rejected(&err);
        assert!(reason.contains("2 symbols"), "{reason}");
        assert!(reason.contains("lines 1, 2"), "{reason}");
        assert_eq!(read(&dir, "a.rs"), source);
    }

    #[test]
    fn an_unindexed_symbol_is_refused() {
        // Arrange
        let (dir, tool) = rig(&[("a.rs", SOURCE)], vec![symbol("keep", "a.rs", 1, 1)]);

        // Act
        let err = tool
            .call(&json!({"symbol": {"path": "a.rs", "name": "target", "new_text": "x"}}))
            .unwrap_err();

        // Assert
        assert!(rejected(&err).1.contains("no symbol named"));
        assert_eq!(read(&dir, "a.rs"), SOURCE);
    }

    #[test]
    fn a_symbol_whose_lines_moved_since_indexing_is_refused() {
        // Arrange
        let (dir, tool) = rig(&[("a.rs", SOURCE)], vec![symbol("target", "a.rs", 1, 2)]);

        // Act
        let err = tool
            .call(&json!({"symbol": {"path": "a.rs", "name": "target", "new_text": "x"}}))
            .unwrap_err();

        // Assert
        assert!(rejected(&err).1.contains("index is out of date"));
        assert_eq!(read(&dir, "a.rs"), SOURCE);
    }

    #[test]
    fn a_symbol_with_an_unknown_range_is_refused() {
        // Arrange
        let (_dir, tool) = rig(&[("a.rs", SOURCE)], vec![symbol("target", "a.rs", 3, 0)]);

        // Act
        let err = tool
            .call(&json!({"symbol": {"path": "a.rs", "name": "target", "new_text": "x"}}))
            .unwrap_err();

        // Assert
        assert!(rejected(&err).1.contains("range"), "{err}");
    }

    #[test]
    fn a_symbol_is_refused_while_the_index_is_stale() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), SOURCE).unwrap();
        let stale = Freshness::Stale {
            files: 1,
            changed: 1,
        };
        let index = FakeIndex(vec![symbol("target", "a.rs", 3, 5)], stale);
        let tool = MultiEditTool::new(Sandbox::new(dir.path()).unwrap(), LIMIT, Arc::new(index));

        // Act
        let err = tool
            .call(&json!({"symbol": {"path": "a.rs", "name": "target", "new_text": "x"}}))
            .unwrap_err();

        // Assert
        assert!(rejected(&err).1.contains("index is out of date"), "{err}");
        assert_eq!(read(&dir, "a.rs"), SOURCE);
    }

    #[test]
    fn a_symbol_in_a_crlf_file_keeps_its_line_endings() {
        // Arrange
        let source = "fn a() {}\r\nfn target() {\r\n    old();\r\n}\r\n";
        let (dir, tool) = rig(&[("a.rs", source)], vec![symbol("target", "a.rs", 2, 4)]);

        // Act
        tool.call(&json!({"symbol": {
            "path": "a.rs", "name": "target", "new_text": "fn target() {\n    new();\n}",
        }}))
        .unwrap();

        // Assert
        assert_eq!(
            read(&dir, "a.rs"),
            "fn a() {}\r\nfn target() {\r\n    new();\r\n}\r\n"
        );
    }

    #[test]
    fn an_empty_new_text_removes_the_symbol() {
        // Arrange
        let (dir, tool) = rig(&[("a.rs", SOURCE)], vec![symbol("target", "a.rs", 3, 5)]);

        // Act
        tool.call(&json!({"symbol": {"path": "a.rs", "name": "target", "new_text": ""}}))
            .unwrap();

        // Assert
        assert_eq!(read(&dir, "a.rs"), "fn keep() {}\n\n\nfn after() {}\n");
    }

    #[test]
    fn a_file_that_changes_after_staging_stops_the_whole_call() {
        // Arrange
        let (dir, tool) = rig(&[("a.txt", "one two"), ("b.txt", "three four")], vec![]);
        let a = dir.path().join("a.txt");
        let tool = tool.with_before_commit(move || fs::write(&a, "changed elsewhere").unwrap());

        // Act
        let err = tool
            .call(&edits(&[
                edit("a.txt", "two", "2"),
                edit("b.txt", "three", "3"),
            ]))
            .unwrap_err();

        // Assert
        assert!(matches!(err, ToolError::Conflict { .. }), "{err}");
        assert_eq!(read(&dir, "a.txt"), "changed elsewhere");
        assert_eq!(read(&dir, "b.txt"), "three four");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn a_rollback_that_fails_names_the_files_left_changed() {
        // Arrange
        let (dir, tool) = rig(&[("a.txt", "one two"), ("b.txt", "three four")], vec![]);
        let root = dir.path().to_path_buf();
        let tool = tool.with_before_commit(move || {
            for entry in fs::read_dir(&root).unwrap() {
                let path = entry.unwrap().path();
                if path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".b.txt")
                {
                    fs::remove_file(path).unwrap();
                }
            }
        });
        let locked = dir.path().to_path_buf();
        let tool = tool.with_before_restore(move || {
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        });

        // Act
        let err = tool
            .call(&edits(&[
                edit("a.txt", "two", "2"),
                edit("b.txt", "three", "3"),
            ]))
            .unwrap_err();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();

        // Assert
        assert!(matches!(err, ToolError::PartlyApplied { .. }), "{err}");
        assert!(err.to_string().contains("a.txt"), "{err}");
    }

    #[test]
    fn the_call_needs_exactly_one_mode() {
        // Arrange
        let (_dir, tool) = rig(&[("a.txt", "x")], vec![]);
        let symbol = json!({"path": "a.txt", "name": "n", "new_text": "t"});

        // Act
        let neither = tool.call(&json!({})).unwrap_err();
        let both = tool
            .call(&json!({"edits": [edit("a.txt", "x", "y")], "symbol": symbol}))
            .unwrap_err();
        let empty = tool.call(&json!({"edits": []})).unwrap_err();
        let blank = tool.call(&edits(&[edit("a.txt", "", "y")])).unwrap_err();

        // Assert
        for err in [neither, both, empty, blank] {
            assert!(matches!(err, ToolError::InvalidInput { .. }), "{err}");
        }
    }
}
