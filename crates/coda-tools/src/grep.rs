// SPDX-License-Identifier: AGPL-3.0-only

//! The `grep` tool: regex search over workspace files.

use std::ffi::OsStr;
use std::fmt::Write;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::sandbox::{EntryKind, Sandbox, SandboxDir, SandboxError};
use crate::{Tool, ToolError, ToolOutput, parse_args};

const NAME: &str = "grep";
const DEFAULT_PATH: &str = ".";
const MAX_DEPTH: usize = 64;
/// Longest line that is matched; longer lines are skipped, never buffered whole.
const MAX_LINE_BYTES: usize = 64 * 1024;
/// Leading bytes inspected for a NUL to classify a file as binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// Files scanned before the walk stops and the result is flagged truncated.
const MAX_FILES_VISITED: usize = 10_000;
/// Directories skipped by default because they are large and rarely useful;
/// `include_dirs` on the tool input opts individual ones back in.
const SKIPPED_DIRS: [&str; 3] = [".git", "target", "node_modules"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    #[serde(default)]
    include_dirs: Vec<String>,
}

type ScanHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// Searches files under a workspace directory for lines matching a regex.
pub struct GrepTool {
    sandbox: Sandbox,
    max_output_bytes: usize,
    max_files_visited: usize,
    before_scan: Option<ScanHook>,
}

impl GrepTool {
    /// Creates the tool, capping each result at `max_output_bytes`.
    #[must_use]
    pub fn new(sandbox: Sandbox, max_output_bytes: usize) -> Self {
        Self {
            sandbox,
            max_output_bytes,
            max_files_visited: MAX_FILES_VISITED,
            before_scan: None,
        }
    }

    /// Runs `hook` with the workspace-relative path just before each file is
    /// opened, so tests can change the tree between listing and opening.
    #[must_use]
    pub fn with_before_scan(mut self, hook: impl Fn(&Path) + Send + Sync + 'static) -> Self {
        self.before_scan = Some(Arc::new(hook));
        self
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let args: GrepArgs = parse_args(NAME, input)?;
        let regex = Regex::new(&args.pattern).map_err(|err| ToolError::InvalidInput {
            tool: NAME,
            reason: err.to_string(),
        })?;
        let start = args.path.as_deref().unwrap_or(DEFAULT_PATH);
        let root = self.sandbox.open_dir(Path::new(start))?;
        let mut search = Search {
            regex: &regex,
            // Collect one byte past the cap so truncation is detectable.
            limit: self.max_output_bytes.saturating_add(1),
            max_files: self.max_files_visited,
            include_dirs: &args.include_dirs,
            before_scan: self.before_scan.as_deref(),
            out: String::new(),
            files: 0,
            skipped_entries: 0,
            skipped_lines: 0,
            stopped_early: false,
        };
        search.walk(&root, Path::new(start), 0)?;
        Ok(search.finish(self.max_output_bytes))
    }
}

struct Search<'a> {
    regex: &'a Regex,
    limit: usize,
    max_files: usize,
    include_dirs: &'a [String],
    before_scan: Option<&'a (dyn Fn(&Path) + Send + Sync)>,
    out: String,
    files: usize,
    skipped_entries: usize,
    skipped_lines: usize,
    stopped_early: bool,
}

/// What one bounded line read produced.
enum Line {
    Eof,
    Complete,
    TooLong,
}

impl Search<'_> {
    fn done(&self) -> bool {
        self.out.len() >= self.limit || self.stopped_early
    }

    fn is_skipped_dir(&self, name: &OsStr) -> bool {
        SKIPPED_DIRS.iter().any(|skipped| name == *skipped)
            && !self.include_dirs.iter().any(|inc| name == inc.as_str())
    }

    /// Turns a per-entry failure into a skip; sandbox violations stay fatal.
    fn tolerate<T>(&mut self, result: Result<T, SandboxError>) -> Result<Option<T>, ToolError> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(
                SandboxError::NotFound(_)
                | SandboxError::Io { .. }
                | SandboxError::NotARegularFile(_)
                | SandboxError::NotADirectory(_),
            ) => {
                self.skipped_entries += 1;
                Ok(None)
            }
            Err(fatal) => Err(fatal.into()),
        }
    }

    fn walk(&mut self, dir: &SandboxDir, rel: &Path, depth: usize) -> Result<(), ToolError> {
        if depth > MAX_DEPTH {
            return Ok(());
        }
        let Some(entries) = self.tolerate(dir.entries())? else {
            return Ok(());
        };
        for entry in entries {
            if self.done() {
                return Ok(());
            }
            let child = display_path(rel, &entry.name);
            match entry.kind {
                EntryKind::Directory if !self.is_skipped_dir(&entry.name) => {
                    if let Some(sub) = self.tolerate(dir.open_dir(&entry.name))? {
                        self.walk(&sub, &child, depth + 1)?;
                    }
                }
                EntryKind::File => {
                    if self.files >= self.max_files {
                        self.stopped_early = true;
                        return Ok(());
                    }
                    self.files += 1;
                    self.scan_file(dir, &entry.name, &child)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn scan_file(&mut self, dir: &SandboxDir, name: &OsStr, shown: &Path) -> Result<(), ToolError> {
        if let Some(hook) = self.before_scan {
            hook(shown);
        }
        let Some(file) = self.tolerate(dir.open_file(name))? else {
            return Ok(());
        };
        if self
            .scan_lines(BufReader::with_capacity(BINARY_SNIFF_BYTES, file), shown)
            .is_err()
        {
            self.skipped_entries += 1;
        }
        Ok(())
    }

    fn scan_lines(&mut self, mut reader: impl BufRead, shown: &Path) -> io::Result<()> {
        if reader.fill_buf()?.contains(&0) {
            return Ok(());
        }
        let mut line = Vec::new();
        let mut number = 0_usize;
        loop {
            number += 1;
            match read_bounded_line(&mut reader, &mut line)? {
                Line::Eof => return Ok(()),
                Line::TooLong => {
                    self.skipped_lines += 1;
                    continue;
                }
                Line::Complete => {}
            }
            if line.contains(&0) {
                return Ok(());
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end_matches(['\n', '\r']);
            if self.regex.is_match(text) {
                let _ = writeln!(self.out, "{}:{number}:{text}", shown.display());
                if self.done() {
                    return Ok(());
                }
            }
        }
    }

    /// Caps the matches and appends a note for anything that was skipped.
    fn finish(self, cap: usize) -> ToolOutput {
        let mut notes = Vec::new();
        if self.skipped_entries > 0 {
            notes.push(format!(
                "skipped {} unreadable entries",
                self.skipped_entries
            ));
        }
        if self.skipped_lines > 0 {
            notes.push(format!("skipped {} over-long lines", self.skipped_lines));
        }
        if self.stopped_early {
            notes.push(format!(
                "search stopped at the {} file limit",
                self.max_files
            ));
        }
        if notes.is_empty() {
            return ToolOutput::capped(self.out, cap);
        }
        let footer = notes.iter().fold(String::new(), |mut acc, note| {
            let _ = writeln!(acc, "[{note}]");
            acc
        });
        let mut output = ToolOutput::capped(self.out, cap.saturating_sub(footer.len()));
        output.text.push_str(&footer);
        output.truncated |= self.stopped_early;
        output
    }
}

/// Reads one line into `line`, holding at most `MAX_LINE_BYTES` of it; the
/// rest of an over-long line is consumed and discarded.
fn read_bounded_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<Line> {
    line.clear();
    let read = reader
        .by_ref()
        .take(MAX_LINE_BYTES as u64)
        .read_until(b'\n', line)?;
    if read == 0 {
        return Ok(Line::Eof);
    }
    if line.last() == Some(&b'\n') || read < MAX_LINE_BYTES {
        return Ok(Line::Complete);
    }
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(Line::TooLong);
        }
        if let Some(at) = buf.iter().position(|&byte| byte == b'\n') {
            reader.consume(at + 1);
            return Ok(Line::TooLong);
        }
        let all = buf.len();
        reader.consume(all);
    }
}

/// Joins with `/`, leaving a bare name when searching from the workspace root.
fn display_path(rel: &Path, name: &OsStr) -> PathBuf {
    if rel == Path::new(DEFAULT_PATH) {
        PathBuf::from(name)
    } else {
        rel.join(name)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::sandbox::SandboxError;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    const BIG_CAP: usize = 4096;

    fn workspace(files: &[(&str, &str)]) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        dir
    }

    fn tool(dir: &TempDir, cap: usize) -> GrepTool {
        GrepTool::new(Sandbox::new(dir.path()).unwrap(), cap)
    }

    #[test]
    fn reports_matching_lines_sorted_by_path() {
        // Arrange
        let dir = workspace(&[
            ("b.txt", "nothing\nfoo two\n"),
            ("a.txt", "foo one\nbar\n"),
            ("sub/c.txt", "xfoo\n"),
        ]);

        // Act
        let out = tool(&dir, BIG_CAP)
            .call(&json!({"pattern": "foo"}))
            .unwrap();

        // Assert
        assert_eq!(
            out.text,
            "a.txt:1:foo one\nb.txt:2:foo two\nsub/c.txt:1:xfoo\n"
        );
        assert!(!out.truncated);
    }

    #[test]
    fn scopes_search_to_a_subdirectory() {
        // Arrange
        let dir = workspace(&[("a.txt", "foo\n"), ("sub/c.txt", "foo\n")]);

        // Act
        let out = tool(&dir, BIG_CAP)
            .call(&json!({"pattern": "foo", "path": "sub"}))
            .unwrap();

        // Assert
        assert_eq!(out.text, "sub/c.txt:1:foo\n");
    }

    #[test]
    fn skips_symlinks_git_directory_and_binary_files() {
        // Arrange
        let dir = workspace(&[
            ("real.txt", "foo\n"),
            (".git/config", "foo\n"),
            ("bin.dat", "foo\0\n"),
        ]);
        symlink(dir.path().join("real.txt"), dir.path().join("alias.txt")).unwrap();

        // Act
        let out = tool(&dir, BIG_CAP)
            .call(&json!({"pattern": "foo"}))
            .unwrap();

        // Assert
        assert_eq!(out.text, "real.txt:1:foo\n");
    }

    #[test]
    fn output_of_exactly_cap_bytes_is_not_truncated() {
        // Arrange
        let dir = workspace(&[("a.txt", "x\n")]);
        let line = "a.txt:1:x\n";

        // Act
        let out = tool(&dir, line.len())
            .call(&json!({"pattern": "x"}))
            .unwrap();

        // Assert
        assert_eq!(out.text, line);
        assert!(!out.truncated);
    }

    #[test]
    fn output_of_cap_plus_one_bytes_is_truncated_to_cap() {
        // Arrange
        let dir = workspace(&[("a.txt", "x\n")]);
        let line = "a.txt:1:x\n";

        // Act
        let out = tool(&dir, line.len() - 1)
            .call(&json!({"pattern": "x"}))
            .unwrap();

        // Assert
        assert_eq!(out.text, &line[..line.len() - 1]);
        assert!(out.truncated);
    }

    #[test]
    fn invalid_regex_is_invalid_input() {
        // Arrange
        let dir = workspace(&[]);

        // Act
        let result = tool(&dir, BIG_CAP).call(&json!({"pattern": "("}));

        // Assert
        assert!(matches!(result, Err(ToolError::InvalidInput { .. })));
    }

    #[test]
    fn rejects_path_outside_workspace() {
        // Arrange
        let dir = workspace(&[]);

        // Act
        let result = tool(&dir, BIG_CAP).call(&json!({"pattern": "x", "path": "../"}));

        // Assert
        assert!(matches!(
            result,
            Err(ToolError::Sandbox(SandboxError::ParentComponent(_)))
        ));
    }

    fn skipped_note(entries: usize) -> String {
        format!("[skipped {entries} unreadable entries]\n")
    }

    /// Returns false when permissions are not enforced (running as root).
    fn deny(path: &std::path::Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        let enforced = fs::read_dir(path).is_err() && fs::File::open(path).is_err();
        if !enforced {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        enforced
    }

    fn restore(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn unreadable_file_is_skipped_and_counted() {
        // Arrange
        let dir = workspace(&[("a.txt", "foo\n"), ("b.txt", "foo\n"), ("c.txt", "foo\n")]);
        let locked = dir.path().join("b.txt");
        if !deny(&locked) {
            return;
        }

        // Act
        let out = tool(&dir, BIG_CAP).call(&json!({"pattern": "foo"}));
        restore(&locked);

        // Assert
        assert_eq!(
            out.unwrap().text,
            format!("a.txt:1:foo\nc.txt:1:foo\n{}", skipped_note(1))
        );
    }

    #[test]
    fn unreadable_directory_is_skipped_and_counted() {
        // Arrange
        let dir = workspace(&[("a.txt", "foo\n"), ("locked/x.txt", "foo\n")]);
        let locked = dir.path().join("locked");
        if !deny(&locked) {
            return;
        }

        // Act
        let out = tool(&dir, BIG_CAP).call(&json!({"pattern": "foo"}));
        restore(&locked);

        // Assert
        assert_eq!(
            out.unwrap().text,
            format!("a.txt:1:foo\n{}", skipped_note(1))
        );
    }

    #[test]
    fn file_deleted_after_listing_is_skipped_and_counted() {
        // Arrange
        let dir = workspace(&[("a.txt", "foo\n"), ("b.txt", "foo\n")]);
        let victim = dir.path().join("a.txt");
        let tool = tool(&dir, BIG_CAP).with_before_scan(move |shown| {
            if shown == Path::new("a.txt") {
                let _ = fs::remove_file(&victim);
            }
        });

        // Act
        let out = tool.call(&json!({"pattern": "foo"})).unwrap();

        // Assert
        assert_eq!(out.text, format!("b.txt:1:foo\n{}", skipped_note(1)));
    }

    #[test]
    fn symlink_swapped_in_after_listing_stays_fatal() {
        // Arrange
        let dir = workspace(&[("a.txt", "foo\n")]);
        let outside = workspace(&[("secret.txt", "foo\n")]);
        let target = outside.path().join("secret.txt");
        let victim = dir.path().join("a.txt");
        let tool = tool(&dir, BIG_CAP).with_before_scan(move |_| {
            let _ = fs::remove_file(&victim);
            symlink(&target, &victim).unwrap();
        });

        // Act
        let result = tool.call(&json!({"pattern": "foo"}));

        // Assert
        assert!(matches!(
            result,
            Err(ToolError::Sandbox(SandboxError::Symlink(_)))
        ));
    }

    #[test]
    fn line_longer_than_the_bound_is_skipped_not_buffered() {
        // Arrange
        let long = "x".repeat(MAX_LINE_BYTES * 2);
        let body = format!("{long}\nfoo\n");
        let dir = workspace(&[("a.txt", &body), ("b.txt", &long)]);

        // Act
        let out = tool(&dir, BIG_CAP)
            .call(&json!({"pattern": "foo|x"}))
            .unwrap();

        // Assert
        assert_eq!(out.text, "a.txt:2:foo\n[skipped 2 over-long lines]\n");
    }

    #[test]
    fn text_before_a_nul_in_the_first_block_is_not_reported() {
        // Arrange
        let dir = workspace(&[("a.bin", "foo\nfoo\n\0tail\n")]);

        // Act
        let out = tool(&dir, BIG_CAP)
            .call(&json!({"pattern": "foo"}))
            .unwrap();

        // Assert
        assert_eq!(out.text, "");
    }

    #[test]
    fn walk_stops_at_the_file_limit_and_flags_truncation() {
        // Arrange
        let dir = workspace(&[("a.txt", "foo\n"), ("b.txt", "foo\n"), ("c.txt", "foo\n")]);
        let mut tool = tool(&dir, BIG_CAP);
        tool.max_files_visited = 2;

        // Act
        let out = tool.call(&json!({"pattern": "foo"})).unwrap();

        // Assert
        assert_eq!(
            out.text,
            "a.txt:1:foo\nb.txt:1:foo\n[search stopped at the 2 file limit]\n"
        );
        assert!(out.truncated);
    }

    #[test]
    fn heavy_directories_are_skipped_unless_included() {
        for name in SKIPPED_DIRS {
            // Arrange
            let inside = format!("{name}/x.txt");
            let dir = workspace(&[("a.txt", "foo\n"), (inside.as_str(), "foo\n")]);

            // Act
            let default = tool(&dir, BIG_CAP)
                .call(&json!({"pattern": "foo"}))
                .unwrap();
            let included = tool(&dir, BIG_CAP)
                .call(&json!({"pattern": "foo", "include_dirs": [name]}))
                .unwrap();

            // Assert
            assert_eq!(default.text, "a.txt:1:foo\n", "{name} not skipped");
            let mut expected = ["a.txt:1:foo".to_owned(), format!("{inside}:1:foo")];
            expected.sort();
            assert_eq!(
                included.text,
                format!("{}\n", expected.join("\n")),
                "{name} not included"
            );
        }
    }
}
