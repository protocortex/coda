// SPDX-License-Identifier: Apache-2.0

//! The `read` tool: a byte range of one workspace file.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::read_log::{FileStamp, ReadLog};
use crate::sandbox::Sandbox;
use crate::{Tool, ToolError, ToolOutput, parse_args};

pub const NAME: &str = "read";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: u64,
    length: Option<u64>,
}

/// Reads a byte range of a file inside the workspace.
pub struct ReadTool {
    sandbox: Sandbox,
    max_output_bytes: usize,
    read_log: Option<ReadLog>,
}

impl ReadTool {
    /// Creates the tool, capping each result at `max_output_bytes`.
    #[must_use]
    pub fn new(sandbox: Sandbox, max_output_bytes: usize) -> Self {
        Self {
            sandbox,
            max_output_bytes,
            read_log: None,
        }
    }

    /// Records every file read in `log`, so `write` knows the session saw it.
    #[must_use]
    pub fn with_read_log(mut self, log: ReadLog) -> Self {
        self.read_log = Some(log);
        self
    }
}

impl Tool for ReadTool {
    fn read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let args: ReadArgs = parse_args(NAME, input)?;
        let io_err = |source| ToolError::Io {
            path: args.path.clone(),
            source,
        };
        let mut file = self.sandbox.open_file(Path::new(&args.path))?;
        let metadata = file.metadata().map_err(io_err)?;
        file.seek(SeekFrom::Start(args.offset)).map_err(io_err)?;
        // One byte past the cap is enough to know the output was cut.
        let cap_plus_one = u64::try_from(self.max_output_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let want = args.length.map_or(cap_plus_one, |n| n.min(cap_plus_one));
        let mut bytes = Vec::new();
        file.take(want).read_to_end(&mut bytes).map_err(io_err)?;
        let whole_file = args.offset == 0 && bytes.len() as u64 == metadata.len();
        let output = ToolOutput::from_bytes(bytes, self.max_output_bytes);
        if let Some(log) = &self.read_log
            && whole_file
            && !output.truncated
        {
            log.record(Path::new(&args.path), FileStamp::of(&metadata));
        }
        Ok(output)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::sandbox::SandboxError;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    const CAP: usize = 8;

    fn tool_with(files: &[(&str, &str)]) -> (TempDir, ReadTool) {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            fs::write(dir.path().join(name), body).unwrap();
        }
        let sandbox = Sandbox::new(dir.path()).unwrap();
        (dir, ReadTool::new(sandbox, CAP))
    }

    #[test]
    fn reads_whole_small_file() {
        // Arrange
        let (_dir, tool) = tool_with(&[("f.txt", "hello")]);

        // Act
        let out = tool.call(&json!({"path": "f.txt"})).unwrap();

        // Assert
        assert_eq!(
            out,
            ToolOutput {
                text: "hello".into(),
                truncated: false
            }
        );
    }

    #[test]
    fn reads_offset_and_length() {
        // Arrange
        let (_dir, tool) = tool_with(&[("f.txt", "0123456789")]);

        // Act
        let out = tool
            .call(&json!({"path": "f.txt", "offset": 2, "length": 3}))
            .unwrap();

        // Assert
        assert_eq!(out.text, "234");
        assert!(!out.truncated);
    }

    #[test]
    fn zero_length_returns_empty() {
        // Arrange
        let (_dir, tool) = tool_with(&[("f.txt", "hello")]);

        // Act
        let out = tool
            .call(&json!({"path": "f.txt", "offset": 1, "length": 0}))
            .unwrap();

        // Assert
        assert_eq!(out.text, "");
        assert!(!out.truncated);
    }

    #[test]
    fn offset_past_end_returns_empty() {
        // Arrange
        let (_dir, tool) = tool_with(&[("f.txt", "hello")]);

        // Act
        let out = tool.call(&json!({"path": "f.txt", "offset": 99})).unwrap();

        // Assert
        assert_eq!(out.text, "");
        assert!(!out.truncated);
    }

    #[test]
    fn output_of_exactly_cap_bytes_is_not_truncated() {
        // Arrange
        let (_dir, tool) = tool_with(&[("f.txt", "12345678")]);

        // Act
        let out = tool.call(&json!({"path": "f.txt"})).unwrap();

        // Assert
        assert_eq!(out.text, "12345678");
        assert!(!out.truncated);
    }

    #[test]
    fn output_of_cap_plus_one_bytes_is_truncated_to_cap() {
        // Arrange
        let (_dir, tool) = tool_with(&[("f.txt", "123456789")]);

        // Act
        let out = tool.call(&json!({"path": "f.txt"})).unwrap();

        // Assert
        assert_eq!(out.text, "12345678");
        assert!(out.truncated);
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        // Arrange: seven ASCII bytes then a two-byte character straddling the cap
        let (_dir, tool) = tool_with(&[("f.txt", "1234567\u{e9}z")]);

        // Act
        let out = tool.call(&json!({"path": "f.txt"})).unwrap();

        // Assert
        assert_eq!(out.text, "1234567");
        assert!(out.truncated);
    }

    #[test]
    fn rejects_parent_traversal() {
        // Arrange
        let (_dir, tool) = tool_with(&[]);

        // Act
        let result = tool.call(&json!({"path": "../etc/passwd"}));

        // Assert
        assert!(matches!(
            result,
            Err(ToolError::Sandbox(SandboxError::ParentComponent(_)))
        ));
    }

    #[test]
    fn missing_path_argument_is_invalid_input() {
        // Arrange
        let (_dir, tool) = tool_with(&[]);

        // Act
        let result = tool.call(&json!({"offset": 1}));

        // Assert
        assert!(matches!(result, Err(ToolError::InvalidInput { .. })));
    }
}
