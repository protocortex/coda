// SPDX-License-Identifier: AGPL-3.0-only

//! The code tools: `code_search`, `code_symbol`, `code_outline` and
//! `code_read_range`, answered from the workspace symbol index.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::sync::Arc;

use coda_protocol::readers::{IndexReader, ReaderError, SearchHit, SymbolLocation};
use serde::Deserialize;
use serde_json::Value;

use crate::sandbox::Sandbox;
use crate::{Tool, ToolError, ToolOutput, parse_args};

pub const SEARCH_NAME: &str = "code_search";
pub const SYMBOL_NAME: &str = "code_symbol";
pub const OUTLINE_NAME: &str = "code_outline";
pub const READ_RANGE_NAME: &str = "code_read_range";

/// Hits `code_search` returns when the caller names no limit.
pub const DEFAULT_SEARCH_LIMIT: usize = 20;
/// Most hits `code_search` ever returns.
pub const MAX_SEARCH_LIMIT: usize = 100;

/// How current the index is relative to the files on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Nothing has been indexed yet.
    Empty,
    /// Every indexed file is unchanged since it was indexed.
    Fresh { files: u64, symbols: u64 },
    /// `changed` files were edited, removed or added since indexing; `files` are indexed.
    Stale { files: u64, changed: u64 },
}

impl fmt::Display for Freshness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "index: empty (no files indexed yet)"),
            Self::Fresh { files, symbols } => {
                write!(f, "index: fresh ({files} files, {symbols} symbols)")
            }
            Self::Stale { files, changed } => write!(
                f,
                "index: stale ({changed} changed, removed or new files since indexing, \
                 {files} files indexed; results may be outdated)"
            ),
        }
    }
}

/// An index the code tools query, with a way to tell how stale it is.
pub trait CodeIndex: IndexReader + Send + Sync {
    /// Compares the index with the files on disk.
    ///
    /// # Errors
    ///
    /// Returns [`ReaderError::Unavailable`] when the index cannot answer.
    fn freshness(&self) -> Result<Freshness, ReaderError>;
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolArgs {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutlineArgs {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeArgs {
    path: String,
    start_line: u64,
    end_line: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum LineRead {
    /// The requested lines, possibly one byte more than the cap.
    Lines(Vec<u8>),
    /// The file ends before the first requested line.
    PastEnd { lines: u64 },
}

/// Collects lines `start..=end` (one-based), stopping once the cap is exceeded.
///
/// No more than `cap + 1` bytes of any line are held, so a huge line costs
/// bounded memory.
fn read_lines(source: impl Read, start: u64, end: u64, cap: usize) -> io::Result<LineRead> {
    let mut reader = BufReader::new(source);
    let mut line = Vec::new();
    let mut out = Vec::new();
    let mut number = 0;
    loop {
        line.clear();
        let read = (&mut reader)
            .take(cap as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        number += 1;
        if number < start {
            if line.last() != Some(&b'\n') && read > cap {
                skip_line(&mut reader)?;
            }
            continue;
        }
        if number > end {
            break;
        }
        out.extend_from_slice(&line);
        // One byte past the cap is enough to know the output was cut.
        if out.len() > cap {
            break;
        }
    }
    if number < start {
        return Ok(LineRead::PastEnd { lines: number });
    }
    Ok(LineRead::Lines(out))
}

/// Consumes the rest of the current line without buffering it.
fn skip_line(reader: &mut impl BufRead) -> io::Result<()> {
    loop {
        let buffered = reader.fill_buf()?;
        if buffered.is_empty() {
            return Ok(());
        }
        let newline = buffered.iter().position(|&b| b == b'\n');
        let used = newline.map_or(buffered.len(), |at| at + 1);
        reader.consume(used);
        if newline.is_some() {
            return Ok(());
        }
    }
}

fn require_text(tool: &'static str, field: &str, value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(ToolError::InvalidInput {
            tool,
            reason: format!("{field} must not be blank"),
        });
    }
    Ok(())
}

fn symbol_line(symbol: &SymbolLocation) -> String {
    format!(
        "{} {} {}:{}\n",
        symbol.kind, symbol.name, symbol.path, symbol.line
    )
}

fn search_line(hit: &SearchHit) -> String {
    format!("{}:{}: {}\n", hit.path, hit.line, hit.snippet)
}

/// Prefixes `body` with the index status line and applies the output cap.
fn with_status(index: &dyn CodeIndex, body: &str, cap: usize) -> Result<ToolOutput, ToolError> {
    let status = index.freshness().map_err(ToolError::Index)?;
    Ok(ToolOutput::capped(format!("{status}\n{body}"), cap))
}

/// Declares a tool that answers from a [`CodeIndex`].
macro_rules! index_tool {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        pub struct $name {
            index: Arc<dyn CodeIndex>,
            max_output_bytes: usize,
        }

        impl $name {
            /// Creates the tool, capping each result at `max_output_bytes`.
            #[must_use]
            pub fn new(index: Arc<dyn CodeIndex>, max_output_bytes: usize) -> Self {
                Self {
                    index,
                    max_output_bytes,
                }
            }
        }
    };
}

index_tool!(
    /// Text search over indexed symbol names.
    CodeSearchTool
);

impl Tool for CodeSearchTool {
    fn name(&self) -> &'static str {
        SEARCH_NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let args: SearchArgs = parse_args(SEARCH_NAME, input)?;
        require_text(SEARCH_NAME, "query", &args.query)?;
        let limit = args
            .limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .clamp(1, MAX_SEARCH_LIMIT);
        let hits = self
            .index
            .search(&args.query, limit)
            .map_err(ToolError::Index)?;
        let body = if hits.is_empty() {
            format!("no matches for `{}`\n", args.query)
        } else {
            hits.iter().map(search_line).collect()
        };
        with_status(self.index.as_ref(), &body, self.max_output_bytes)
    }
}

index_tool!(
    /// Finds where a symbol is defined.
    CodeSymbolTool
);

impl Tool for CodeSymbolTool {
    fn name(&self) -> &'static str {
        SYMBOL_NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let args: SymbolArgs = parse_args(SYMBOL_NAME, input)?;
        require_text(SYMBOL_NAME, "name", &args.name)?;
        // The index matches by name prefix; this tool promises the exact name.
        let found: Vec<_> = self
            .index
            .symbols(&args.name)
            .map_err(ToolError::Index)?
            .into_iter()
            .filter(|symbol| symbol.name == args.name)
            .collect();
        let body = if found.is_empty() {
            format!("no symbol named `{}` is indexed\n", args.name)
        } else {
            found.iter().map(symbol_line).collect()
        };
        with_status(self.index.as_ref(), &body, self.max_output_bytes)
    }
}

index_tool!(
    /// Lists the symbols of one file in source order.
    CodeOutlineTool
);

impl Tool for CodeOutlineTool {
    fn name(&self) -> &'static str {
        OUTLINE_NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let args: OutlineArgs = parse_args(OUTLINE_NAME, input)?;
        let found = self.index.outline(&args.path).map_err(ToolError::Index)?;
        let body = if found.is_empty() {
            format!("no symbols are indexed for {}\n", args.path)
        } else {
            found.iter().map(symbol_line).collect()
        };
        with_status(self.index.as_ref(), &body, self.max_output_bytes)
    }
}

/// Reads an inclusive line range of one workspace file.
pub struct CodeReadRangeTool {
    sandbox: Sandbox,
    max_output_bytes: usize,
}

impl CodeReadRangeTool {
    /// Creates the tool, capping each result at `max_output_bytes`.
    #[must_use]
    pub fn new(sandbox: Sandbox, max_output_bytes: usize) -> Self {
        Self {
            sandbox,
            max_output_bytes,
        }
    }
}

impl Tool for CodeReadRangeTool {
    fn name(&self) -> &'static str {
        READ_RANGE_NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let args: RangeArgs = parse_args(READ_RANGE_NAME, input)?;
        if args.start_line == 0 || args.end_line < args.start_line {
            return Err(ToolError::InvalidInput {
                tool: READ_RANGE_NAME,
                reason: "start_line must be at least 1 and no greater than end_line".to_owned(),
            });
        }
        let file = self.sandbox.open_file(Path::new(&args.path))?;
        let io_err = |source| ToolError::Io {
            path: args.path.clone(),
            source,
        };
        let read = read_lines(file, args.start_line, args.end_line, self.max_output_bytes)
            .map_err(io_err)?;
        match read {
            LineRead::Lines(bytes) => Ok(ToolOutput::from_bytes(bytes, self.max_output_bytes)),
            LineRead::PastEnd { lines } => Err(ToolError::PastEndOfFile {
                path: args.path,
                start: args.start_line,
                lines,
            }),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::sandbox::SandboxError;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    const BIG_CAP: usize = 64 * 1024;
    const FRESH: Freshness = Freshness::Fresh {
        files: 2,
        symbols: 3,
    };

    struct FakeIndex {
        symbols: Vec<SymbolLocation>,
        hits: Vec<SearchHit>,
        freshness: Result<Freshness, String>,
    }

    impl IndexReader for FakeIndex {
        fn symbols(&self, query: &str) -> Result<Vec<SymbolLocation>, ReaderError> {
            Ok(self
                .symbols
                .iter()
                .filter(|s| s.name.starts_with(query))
                .cloned()
                .collect())
        }

        fn outline(&self, path: &str) -> Result<Vec<SymbolLocation>, ReaderError> {
            Ok(self
                .symbols
                .iter()
                .filter(|s| s.path == path)
                .cloned()
                .collect())
        }

        fn search(&self, _query: &str, limit: usize) -> Result<Vec<SearchHit>, ReaderError> {
            Ok(self.hits.iter().take(limit).cloned().collect())
        }
    }

    impl CodeIndex for FakeIndex {
        fn freshness(&self) -> Result<Freshness, ReaderError> {
            self.freshness.clone().map_err(ReaderError::Unavailable)
        }
    }

    fn location(name: &str, path: &str, line: u32) -> SymbolLocation {
        SymbolLocation {
            name: name.to_owned(),
            kind: "fn".to_owned(),
            path: path.to_owned(),
            line,
        }
    }

    fn index(freshness: Freshness) -> Arc<dyn CodeIndex> {
        Arc::new(FakeIndex {
            symbols: vec![
                location("run", "src/a.rs", 3),
                location("stop", "src/a.rs", 9),
                location("run", "src/b.rs", 1),
                location("runner", "src/c.rs", 5),
            ],
            hits: (1..=150)
                .map(|n| SearchHit {
                    path: "src/a.rs".to_owned(),
                    line: n,
                    snippet: format!("hit{n}"),
                })
                .collect(),
            freshness: Ok(freshness),
        })
    }

    fn range_tool(files: &[(&str, &str)], cap: usize) -> (TempDir, CodeReadRangeTool) {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            fs::write(dir.path().join(name), body).unwrap();
        }
        let sandbox = Sandbox::new(dir.path()).unwrap();
        (dir, CodeReadRangeTool::new(sandbox, cap))
    }

    #[test]
    fn symbol_lists_every_definition_under_a_fresh_status() {
        // Arrange
        let tool = CodeSymbolTool::new(index(FRESH), BIG_CAP);

        // Act
        let out = tool.call(&json!({"name": "run"})).unwrap();

        // Assert
        assert_eq!(
            out.text,
            "index: fresh (2 files, 3 symbols)\nfn run src/a.rs:3\nfn run src/b.rs:1\n"
        );
        assert!(!out.truncated);
    }

    #[test]
    fn unknown_symbol_says_so_instead_of_failing() {
        // Arrange
        let tool = CodeSymbolTool::new(index(FRESH), BIG_CAP);

        // Act
        let out = tool.call(&json!({"name": "missing"})).unwrap();

        // Assert
        assert_eq!(
            out.text,
            "index: fresh (2 files, 3 symbols)\nno symbol named `missing` is indexed\n"
        );
    }

    #[test]
    fn outline_lists_one_file_in_order() {
        // Arrange
        let tool = CodeOutlineTool::new(index(FRESH), BIG_CAP);

        // Act
        let out = tool.call(&json!({"path": "src/a.rs"})).unwrap();

        // Assert
        assert_eq!(
            out.text,
            "index: fresh (2 files, 3 symbols)\nfn run src/a.rs:3\nfn stop src/a.rs:9\n"
        );
    }

    #[test]
    fn outline_of_an_unindexed_file_says_so() {
        // Arrange
        let tool = CodeOutlineTool::new(index(FRESH), BIG_CAP);

        // Act
        let out = tool.call(&json!({"path": "src/none.rs"})).unwrap();

        // Assert
        assert!(
            out.text
                .ends_with("no symbols are indexed for src/none.rs\n")
        );
    }

    #[test]
    fn search_formats_hits_and_bounds_the_limit() {
        // Arrange
        let tool = CodeSearchTool::new(index(FRESH), BIG_CAP);
        let cases = [
            (json!({"query": "hit"}), DEFAULT_SEARCH_LIMIT),
            (json!({"query": "hit", "limit": 5000}), MAX_SEARCH_LIMIT),
        ];

        for (input, expected) in cases {
            // Act
            let out = tool.call(&input).unwrap();

            // Assert
            let lines: Vec<&str> = out.text.lines().collect();
            assert_eq!(lines[1], "src/a.rs:1: hit1");
            assert_eq!(lines.len(), 1 + expected);
        }
    }

    #[test]
    fn blank_search_query_is_invalid_input() {
        // Arrange
        let tool = CodeSearchTool::new(index(FRESH), BIG_CAP);

        // Act
        let result = tool.call(&json!({"query": "  "}));

        // Assert
        assert!(matches!(result, Err(ToolError::InvalidInput { .. })));
    }

    #[test]
    fn stale_and_empty_indexes_are_reported_in_the_status_line() {
        // Arrange
        let stale = Freshness::Stale {
            files: 4,
            changed: 1,
        };
        let cases = [
            (
                stale,
                "index: stale (1 changed, removed or new files since indexing, 4 files",
            ),
            (Freshness::Empty, "index: empty"),
        ];

        for (freshness, prefix) in cases {
            let tool = CodeSymbolTool::new(index(freshness), BIG_CAP);

            // Act
            let out = tool.call(&json!({"name": "run"})).unwrap();

            // Assert
            assert!(out.text.starts_with(prefix), "{}", out.text);
        }
    }

    #[test]
    fn unavailable_index_is_a_tool_error() {
        // Arrange
        let broken: Arc<dyn CodeIndex> = Arc::new(FakeIndex {
            symbols: Vec::new(),
            hits: Vec::new(),
            freshness: Err("building".to_owned()),
        });
        let tool = CodeSymbolTool::new(broken, BIG_CAP);

        // Act
        let result = tool.call(&json!({"name": "run"}));

        // Assert
        assert!(matches!(result, Err(ToolError::Index(_))));
    }

    #[test]
    fn index_output_of_exactly_cap_bytes_is_not_truncated_and_one_over_is() {
        // Arrange
        let full = CodeSymbolTool::new(index(FRESH), BIG_CAP)
            .call(&json!({"name": "run"}))
            .unwrap()
            .text;
        let exact = CodeSymbolTool::new(index(FRESH), full.len());
        let short = CodeSymbolTool::new(index(FRESH), full.len() - 1);

        // Act
        let at_cap = exact.call(&json!({"name": "run"})).unwrap();
        let over_cap = short.call(&json!({"name": "run"})).unwrap();

        // Assert
        assert_eq!(at_cap.text, full);
        assert!(!at_cap.truncated);
        assert_eq!(over_cap.text, full[..full.len() - 1]);
        assert!(over_cap.truncated);
    }

    #[test]
    fn symbol_returns_exact_name_matches_only() {
        // Arrange
        let tool = CodeSymbolTool::new(index(FRESH), BIG_CAP);

        // Act
        let out = tool.call(&json!({"name": "run"})).unwrap();

        // Assert
        assert!(!out.text.contains("runner"), "{}", out.text);
    }

    /// Yields `limit` bytes of one endless line, then fails.
    struct EndlessLine {
        served: usize,
        limit: usize,
    }

    impl Read for EndlessLine {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.served >= self.limit {
                return Err(io::Error::other("read past the memory bound"));
            }
            let n = buf.len().min(self.limit - self.served);
            buf[..n].fill(b'x');
            self.served += n;
            Ok(n)
        }
    }

    #[test]
    fn range_over_a_huge_line_reads_only_a_bounded_prefix() {
        // Arrange
        let source = EndlessLine {
            served: 0,
            limit: 64 * 1024,
        };

        // Act
        let read = read_lines(source, 1, 1, 16).unwrap();

        // Assert
        assert_eq!(read, LineRead::Lines(vec![b'x'; 17]));
    }

    #[test]
    fn range_skips_a_huge_line_before_the_start_to_count_lines() {
        // Arrange
        let mut text = "x".repeat(100_000);
        text.push_str("\nsecond\n");

        // Act
        let read = read_lines(text.as_bytes(), 2, 2, 16).unwrap();

        // Assert
        assert_eq!(read, LineRead::Lines(b"second\n".to_vec()));
    }

    #[test]
    fn range_reads_inclusive_lines() {
        // Arrange
        let (_dir, tool) = range_tool(&[("f.rs", "a\nb\nc\nd\n")], BIG_CAP);

        // Act
        let out = tool
            .call(&json!({"path": "f.rs", "start_line": 2, "end_line": 3}))
            .unwrap();

        // Assert
        assert_eq!(out.text, "b\nc\n");
        assert!(!out.truncated);
    }

    #[test]
    fn range_end_past_eof_is_clamped() {
        // Arrange
        let (_dir, tool) = range_tool(&[("f.rs", "a\nb\nc")], BIG_CAP);

        // Act
        let out = tool
            .call(&json!({"path": "f.rs", "start_line": 2, "end_line": 99}))
            .unwrap();

        // Assert
        assert_eq!(out.text, "b\nc");
    }

    #[test]
    fn range_start_past_eof_names_the_line_count() {
        // Arrange
        let (_dir, tool) = range_tool(&[("f.rs", "a\nb\n")], BIG_CAP);

        // Act
        let result = tool.call(&json!({"path": "f.rs", "start_line": 3, "end_line": 4}));

        // Assert
        assert!(
            matches!(
                result,
                Err(ToolError::PastEndOfFile {
                    start: 3,
                    lines: 2,
                    ..
                })
            ),
            "{result:?}"
        );
    }

    #[test]
    fn range_on_an_empty_file_is_past_eof() {
        // Arrange
        let (_dir, tool) = range_tool(&[("f.rs", "")], BIG_CAP);

        // Act
        let result = tool.call(&json!({"path": "f.rs", "start_line": 1, "end_line": 1}));

        // Assert
        assert!(matches!(
            result,
            Err(ToolError::PastEndOfFile { lines: 0, .. })
        ));
    }

    #[test]
    fn range_with_zero_start_or_inverted_bounds_is_invalid_input() {
        // Arrange
        let (_dir, tool) = range_tool(&[("f.rs", "a\n")], BIG_CAP);

        // Act
        let zero = tool.call(&json!({"path": "f.rs", "start_line": 0, "end_line": 1}));
        let inverted = tool.call(&json!({"path": "f.rs", "start_line": 3, "end_line": 2}));

        // Assert
        assert!(matches!(zero, Err(ToolError::InvalidInput { .. })));
        assert!(matches!(inverted, Err(ToolError::InvalidInput { .. })));
    }

    #[test]
    fn range_output_of_exactly_cap_bytes_is_not_truncated_and_one_over_is() {
        // Arrange
        let body = "12\n34\n";
        let (_d1, exact) = range_tool(&[("f.rs", body)], body.len());
        let (_d2, short) = range_tool(&[("f.rs", body)], body.len() - 1);
        let args = json!({"path": "f.rs", "start_line": 1, "end_line": 2});

        // Act
        let at_cap = exact.call(&args).unwrap();
        let over_cap = short.call(&args).unwrap();

        // Assert
        assert_eq!(at_cap.text, body);
        assert!(!at_cap.truncated);
        assert_eq!(over_cap.text, "12\n34");
        assert!(over_cap.truncated);
    }

    #[test]
    fn range_refuses_a_symlinked_file_like_read() {
        // Arrange
        let (dir, tool) = range_tool(&[("real.rs", "secret\n")], BIG_CAP);
        std::os::unix::fs::symlink("real.rs", dir.path().join("link.rs")).unwrap();

        // Act
        let result = tool.call(&json!({"path": "link.rs", "start_line": 1, "end_line": 1}));

        // Assert
        assert!(matches!(
            result,
            Err(ToolError::Sandbox(SandboxError::Symlink(_)))
        ));
    }

    #[test]
    fn range_refuses_a_symlink_pointing_outside_the_workspace() {
        // Arrange
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("x.rs"), "secret\n").unwrap();
        let (dir, tool) = range_tool(&[], BIG_CAP);
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();

        // Act
        let result = tool.call(&json!({"path": "out/x.rs", "start_line": 1, "end_line": 1}));

        // Assert
        assert!(matches!(result, Err(ToolError::Sandbox(_))), "{result:?}");
    }

    #[test]
    fn range_refuses_absolute_and_parent_paths() {
        // Arrange
        let (_dir, tool) = range_tool(&[], BIG_CAP);

        // Act
        let absolute = tool.call(&json!({"path": "/etc/passwd", "start_line": 1, "end_line": 1}));
        let parent = tool.call(&json!({"path": "../x", "start_line": 1, "end_line": 1}));

        // Assert
        assert!(matches!(
            absolute,
            Err(ToolError::Sandbox(SandboxError::Absolute(_)))
        ));
        assert!(matches!(
            parent,
            Err(ToolError::Sandbox(SandboxError::ParentComponent(_)))
        ));
    }
}
