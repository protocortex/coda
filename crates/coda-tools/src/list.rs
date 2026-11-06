// SPDX-License-Identifier: Apache-2.0

//! The `list` tool: directory entries, or the names in a tree that match a glob.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;
use serde_json::Value;

use crate::grep::is_skipped_dir;
use crate::sandbox::{EntryKind, Sandbox, SandboxDir};
use crate::{Tool, ToolError, ToolOutput, parse_args};

pub const NAME: &str = "list";
const DEFAULT_PATH: &str = ".";
const MAX_DEPTH: usize = 64;
/// Entries returned before the listing stops and is flagged truncated.
const MAX_ENTRIES: usize = 1_000;
/// Entries examined by a pattern search before it stops.
const MAX_VISITED: usize = 20_000;
const DIR_SUFFIX: char = '/';
const EMPTY: &str = "no entries";
const TRUNCATED_NOTE: &str = "[more entries exist; narrow the path or the pattern]";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    path: Option<String>,
    pattern: Option<String>,
    #[serde(default)]
    include_dirs: Vec<String>,
}

/// Lists a workspace directory, or searches its tree by name pattern.
pub struct ListTool {
    sandbox: Sandbox,
    max_output_bytes: usize,
    max_entries: usize,
}

impl ListTool {
    /// Creates the tool, capping each result at `max_output_bytes`.
    #[must_use]
    pub fn new(sandbox: Sandbox, max_output_bytes: usize) -> Self {
        Self {
            sandbox,
            max_output_bytes,
            max_entries: MAX_ENTRIES,
        }
    }

    /// Stops after `max_entries` entries instead of the default.
    #[must_use]
    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries;
        self
    }
}

impl Tool for ListTool {
    fn name(&self) -> &'static str {
        NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let args: ListArgs = parse_args(NAME, input)?;
        let start = args.path.as_deref().unwrap_or(DEFAULT_PATH);
        let matcher = args.pattern.as_deref().map(compile).transpose()?;
        let root = self.sandbox.open_dir(Path::new(start))?;
        let mut listing = Listing {
            matcher: matcher.as_ref(),
            include_dirs: &args.include_dirs,
            max_entries: self.max_entries,
            lines: Vec::new(),
            visited: 0,
            truncated: false,
        };
        listing.walk(&root, Path::new(start), Path::new(""), 0)?;
        Ok(listing.finish(self.max_output_bytes))
    }
}

/// A pattern with a `/` is matched against the path below the listed
/// directory, any other against the entry name alone.
struct Pattern {
    matcher: GlobMatcher,
    whole_path: bool,
}

fn compile(pattern: &str) -> Result<Pattern, ToolError> {
    let glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|err| ToolError::InvalidInput {
            tool: NAME,
            reason: err.to_string(),
        })?;
    Ok(Pattern {
        matcher: glob.compile_matcher(),
        whole_path: pattern.contains('/'),
    })
}

struct Listing<'a> {
    matcher: Option<&'a Pattern>,
    include_dirs: &'a [String],
    max_entries: usize,
    lines: Vec<String>,
    visited: usize,
    truncated: bool,
}

impl Listing<'_> {
    /// `shown` is the workspace-relative path of `dir`, `below` its path under
    /// the listed directory.
    fn walk(
        &mut self,
        dir: &SandboxDir,
        shown: &Path,
        below: &Path,
        depth: usize,
    ) -> Result<(), ToolError> {
        if depth > MAX_DEPTH {
            self.truncated = true;
            return Ok(());
        }
        let entries = match dir.entries() {
            Ok(entries) => entries,
            // The directory asked for is an error; one met on the way is left out.
            Err(error) if depth == 0 => return Err(error.into()),
            Err(_) => return Ok(()),
        };
        for entry in entries {
            if self.truncated {
                return Ok(());
            }
            let is_dir = entry.kind == EntryKind::Directory;
            if is_dir && is_skipped_dir(&entry.name, self.include_dirs) {
                continue;
            }
            self.visited += 1;
            if self.visited > MAX_VISITED {
                self.truncated = true;
                return Ok(());
            }
            let child_below = below.join(&entry.name);
            if self.matches(&entry.name, &child_below) {
                self.push(&join_shown(shown, &entry.name), is_dir);
            }
            if is_dir
                && self.matcher.is_some()
                && let Ok(sub) = dir.open_dir(&entry.name)
            {
                self.walk(
                    &sub,
                    &join_shown(shown, &entry.name),
                    &child_below,
                    depth + 1,
                )?;
            }
        }
        Ok(())
    }

    fn matches(&self, name: &OsStr, below: &Path) -> bool {
        match self.matcher {
            None => true,
            Some(pattern) if pattern.whole_path => pattern.matcher.is_match(below),
            Some(pattern) => pattern.matcher.is_match(name),
        }
    }

    fn push(&mut self, path: &Path, is_dir: bool) {
        if self.lines.len() >= self.max_entries {
            self.truncated = true;
            return;
        }
        let mut line = path.display().to_string();
        if is_dir {
            line.push(DIR_SUFFIX);
        }
        self.lines.push(line);
    }

    fn finish(self, max_output_bytes: usize) -> ToolOutput {
        let mut text = if self.lines.is_empty() {
            EMPTY.to_owned()
        } else {
            self.lines.join("\n")
        };
        if self.truncated {
            text.push('\n');
            text.push_str(TRUNCATED_NOTE);
        }
        let mut out = ToolOutput::capped(text, max_output_bytes);
        out.truncated |= self.truncated;
        out
    }
}

fn join_shown(shown: &Path, name: &OsStr) -> PathBuf {
    if shown == Path::new(DEFAULT_PATH) {
        PathBuf::from(name)
    } else {
        shown.join(name)
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

    const CAP: usize = 64 * 1024;

    fn tree(files: &[&str]) -> (TempDir, ListTool) {
        let dir = tempfile::tempdir().unwrap();
        for file in files {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "x").unwrap();
        }
        let sandbox = Sandbox::new(dir.path()).unwrap();
        (dir, ListTool::new(sandbox, CAP))
    }

    fn lines(out: &ToolOutput) -> Vec<&str> {
        out.text.lines().collect()
    }

    #[test]
    fn lists_one_level_with_directories_marked() {
        // Arrange
        let (_dir, tool) = tree(&["b.txt", "a.txt", "src/lib.rs"]);

        // Act
        let out = tool.call(&json!({})).unwrap();

        // Assert
        assert_eq!(lines(&out), ["a.txt", "b.txt", "src/"]);
        assert!(!out.truncated);
    }

    #[test]
    fn lists_a_subdirectory_with_workspace_relative_names() {
        // Arrange
        let (_dir, tool) = tree(&["src/lib.rs", "src/deep/mod.rs"]);

        // Act
        let out = tool.call(&json!({"path": "src"})).unwrap();

        // Assert
        assert_eq!(lines(&out), ["src/deep/", "src/lib.rs"]);
    }

    #[test]
    fn skips_heavy_directories_unless_included() {
        // Arrange
        let (_dir, tool) = tree(&["a.txt", "target/out.bin", "node_modules/m.js", ".git/HEAD"]);

        // Act
        let plain = tool.call(&json!({})).unwrap();
        let included = tool.call(&json!({"include_dirs": ["target"]})).unwrap();

        // Assert
        assert_eq!(lines(&plain), ["a.txt"]);
        assert_eq!(lines(&included), ["a.txt", "target/"]);
    }

    #[test]
    fn a_name_pattern_matches_at_any_depth() {
        // Arrange
        let (_dir, tool) = tree(&[
            "a.rs",
            "src/b.rs",
            "src/deep/c.rs",
            "src/d.txt",
            "target/e.rs",
        ]);

        // Act
        let out = tool.call(&json!({"pattern": "*.rs"})).unwrap();

        // Assert
        assert_eq!(lines(&out), ["a.rs", "src/b.rs", "src/deep/c.rs"]);
    }

    #[test]
    fn a_path_pattern_matches_below_the_listed_directory() {
        // Arrange
        let (_dir, tool) = tree(&["a.rs", "src/b.rs", "src/deep/c.rs", "other/d.rs"]);

        // Act
        let all = tool.call(&json!({"pattern": "src/**/*.rs"})).unwrap();
        let one_level = tool.call(&json!({"pattern": "src/*.rs"})).unwrap();
        let from_src = tool
            .call(&json!({"path": "src", "pattern": "deep/*.rs"}))
            .unwrap();

        // Assert
        assert_eq!(lines(&all), ["src/b.rs", "src/deep/c.rs"]);
        assert_eq!(lines(&one_level), ["src/b.rs"]);
        assert_eq!(lines(&from_src), ["src/deep/c.rs"]);
    }

    #[test]
    fn a_pattern_with_no_match_says_so() {
        // Arrange
        let (_dir, tool) = tree(&["a.txt"]);

        // Act
        let out = tool.call(&json!({"pattern": "*.rs"})).unwrap();

        // Assert
        assert_eq!(out.text, EMPTY);
    }

    #[test]
    fn stops_at_the_entry_limit_and_says_so() {
        // Arrange
        let (_dir, tool) = tree(&["a", "b", "c", "d"]);
        let tool = tool.with_max_entries(2);

        // Act
        let out = tool.call(&json!({})).unwrap();

        // Assert
        assert_eq!(lines(&out), ["a", "b", TRUNCATED_NOTE]);
        assert!(out.truncated);
    }

    #[test]
    fn a_tree_deeper_than_the_walk_limit_is_flagged_truncated() {
        // Arrange
        let deep = vec!["d"; MAX_DEPTH + 2].join("/");
        let (_dir, tool) = tree(&[&format!("{deep}/x.rs")]);

        // Act
        let out = tool.call(&json!({"pattern": "*.rs"})).unwrap();

        // Assert
        assert!(out.truncated);
        assert!(out.text.ends_with(TRUNCATED_NOTE), "{}", out.text);
    }

    #[test]
    fn refuses_paths_outside_the_workspace() {
        // Arrange
        let (dir, tool) = tree(&["a.txt"]);
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("link")).unwrap();

        // Act
        let parent = tool.call(&json!({"path": ".."})).unwrap_err();
        let absolute = tool.call(&json!({"path": "/"})).unwrap_err();
        let linked = tool.call(&json!({"path": "link"})).unwrap_err();

        // Assert
        assert!(matches!(
            parent,
            ToolError::Sandbox(SandboxError::ParentComponent(_))
        ));
        assert!(matches!(
            absolute,
            ToolError::Sandbox(SandboxError::Absolute(_))
        ));
        assert!(matches!(
            linked,
            ToolError::Sandbox(SandboxError::Symlink(_) | SandboxError::OutsideRoot(_))
        ));
    }

    #[test]
    fn does_not_follow_a_symlinked_directory_inside_the_tree() {
        // Arrange
        let (dir, tool) = tree(&["a.txt"]);
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.rs"), "x").unwrap();
        symlink(outside.path(), dir.path().join("link")).unwrap();

        // Act
        let out = tool.call(&json!({"pattern": "*.rs"})).unwrap();

        // Assert
        assert_eq!(out.text, EMPTY);
    }

    #[test]
    fn refuses_a_file_and_a_bad_pattern() {
        // Arrange
        let (_dir, tool) = tree(&["a.txt"]);

        // Act
        let file = tool.call(&json!({"path": "a.txt"})).unwrap_err();
        let bad = tool.call(&json!({"pattern": "[a"})).unwrap_err();

        // Assert
        assert!(matches!(
            file,
            ToolError::Sandbox(SandboxError::NotADirectory(_))
        ));
        assert!(matches!(bad, ToolError::InvalidInput { .. }));
    }
}
