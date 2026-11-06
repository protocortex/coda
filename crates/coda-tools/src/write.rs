// SPDX-License-Identifier: Apache-2.0

//! The `write` tool: create a workspace file, or replace one the session has read.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use rustix::fs::{Mode, fstat};
use serde::Deserialize;
use serde_json::Value;

use crate::edit::{Original, replace_file};
use crate::read_log::{FileStamp, ReadLog};
use crate::sandbox::{Sandbox, SandboxDir, SandboxError};
use crate::{Tool, ToolError, ToolOutput, parse_args};

pub const NAME: &str = "write";

/// Permission bits of a new file (rw-r--r--), before the umask.
const NEW_FILE_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::RGRP)
    .union(Mode::ROTH);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

/// Creates a file, or replaces one only when this session read its current content.
pub struct WriteTool {
    sandbox: Sandbox,
    max_file_bytes: usize,
    read_log: ReadLog,
}

impl WriteTool {
    /// Creates the tool, refusing content larger than `max_file_bytes`.
    #[must_use]
    pub fn new(sandbox: Sandbox, max_file_bytes: usize, read_log: ReadLog) -> Self {
        Self {
            sandbox,
            max_file_bytes,
            read_log,
        }
    }

    fn replace(
        &self,
        dir: &SandboxDir,
        name: &OsStr,
        file: &File,
        args: &WriteArgs,
    ) -> Result<(), ToolError> {
        let io_err = |source| ToolError::Io {
            path: args.path.clone(),
            source,
        };
        let stat = fstat(file).map_err(|e| io_err(e.into()))?;
        let snapshot = file.metadata().map_err(io_err)?;
        match self.read_log.stamp(Path::new(&args.path)) {
            None => {
                return Err(ToolError::NotRead {
                    path: args.path.clone(),
                });
            }
            Some(seen) if seen != FileStamp::of(&snapshot) => {
                return Err(ToolError::Conflict {
                    path: args.path.clone(),
                });
            }
            Some(_) => {}
        }
        let original = Original {
            file,
            stat: &stat,
            snapshot: &snapshot,
        };
        replace_file(dir, name, &original, args.content.as_bytes(), &args.path)
    }

    /// Notes the file as it is now, so the session can write it again without
    /// reading what it just wrote.
    fn remember(&self, dir: &SandboxDir, name: &OsStr, path: &str) {
        let stamp = dir
            .open_file(name)
            .ok()
            .and_then(|file| file.metadata().ok())
            .map(|metadata| FileStamp::of(&metadata));
        if let Some(stamp) = stamp {
            self.read_log.record(Path::new(path), stamp);
        }
    }
}

fn create(dir: &SandboxDir, name: &OsStr, args: &WriteArgs) -> Result<(), ToolError> {
    let mut file = dir.create_new(name, NEW_FILE_MODE)?;
    let written = file
        .write_all(args.content.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(source) = written {
        // Best effort: the write error is the one worth reporting.
        let _ = dir.remove(name);
        return Err(ToolError::Io {
            path: args.path.clone(),
            source,
        });
    }
    Ok(())
}

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let args: WriteArgs = parse_args(NAME, input)?;
        if args.content.len() > self.max_file_bytes {
            return Err(ToolError::TooLarge {
                path: args.path,
                limit: self.max_file_bytes,
            });
        }
        let (dir, name) = self.sandbox.open_parent(Path::new(&args.path))?;
        let verb = match dir.open_file(&name) {
            Ok(file) => {
                self.replace(&dir, &name, &file, &args)?;
                "wrote"
            }
            Err(SandboxError::NotFound(_)) => {
                create(&dir, &name, &args)?;
                "created"
            }
            Err(other) => return Err(other.into()),
        };
        self.remember(&dir, &name, &args.path);
        Ok(ToolOutput {
            text: format!("{verb} {} ({} bytes)", args.path, args.content.len()),
            truncated: false,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::read::ReadTool;
    use crate::sandbox::SandboxError;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    const LIMIT: usize = 64;
    const READ_CAP: usize = 1024;

    struct Rig {
        dir: TempDir,
        write: WriteTool,
        read: ReadTool,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let log = ReadLog::new();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        Rig {
            write: WriteTool::new(sandbox.clone(), LIMIT, log.clone()),
            read: ReadTool::new(sandbox, READ_CAP).with_read_log(log),
            dir,
        }
    }

    fn put(path: &str, content: &str) -> Value {
        json!({"path": path, "content": content})
    }

    #[test]
    fn creates_a_new_file() {
        // Arrange
        let rig = rig();

        // Act
        let out = rig.write.call(&put("new.txt", "hello")).unwrap();

        // Assert
        assert_eq!(out.text, "created new.txt (5 bytes)");
        assert_eq!(fs::read(rig.dir.path().join("new.txt")).unwrap(), b"hello");
    }

    #[test]
    fn refuses_to_replace_a_file_that_was_not_read() {
        // Arrange
        let rig = rig();
        fs::write(rig.dir.path().join("f.txt"), "old").unwrap();

        // Act
        let err = rig.write.call(&put("f.txt", "new")).unwrap_err();

        // Assert
        assert!(matches!(err, ToolError::NotRead { .. }), "{err}");
        assert_eq!(fs::read(rig.dir.path().join("f.txt")).unwrap(), b"old");
    }

    #[test]
    fn replaces_a_file_that_was_read() {
        // Arrange
        let rig = rig();
        fs::write(rig.dir.path().join("f.txt"), "old").unwrap();
        rig.read.call(&json!({"path": "./f.txt"})).unwrap();

        // Act
        let out = rig.write.call(&put("f.txt", "new")).unwrap();

        // Assert
        assert_eq!(out.text, "wrote f.txt (3 bytes)");
        assert_eq!(fs::read(rig.dir.path().join("f.txt")).unwrap(), b"new");
    }

    #[test]
    fn a_partial_read_does_not_count_as_reading_the_file() {
        // Arrange
        let rig = rig();
        fs::write(rig.dir.path().join("f.txt"), "old content").unwrap();
        rig.read
            .call(&json!({"path": "f.txt", "offset": 0, "length": 3}))
            .unwrap();
        rig.read
            .call(&json!({"path": "f.txt", "offset": 3}))
            .unwrap();

        // Act
        let err = rig.write.call(&put("f.txt", "new")).unwrap_err();

        // Assert
        assert!(matches!(err, ToolError::NotRead { .. }), "{err}");
    }

    #[test]
    fn refuses_to_replace_a_file_that_changed_after_it_was_read() {
        // Arrange
        let rig = rig();
        let path = rig.dir.path().join("f.txt");
        fs::write(&path, "old").unwrap();
        rig.read.call(&json!({"path": "f.txt"})).unwrap();
        fs::write(&path, "changed by someone else").unwrap();

        // Act
        let err = rig.write.call(&put("f.txt", "new")).unwrap_err();

        // Assert
        assert!(matches!(err, ToolError::Conflict { .. }), "{err}");
        assert_eq!(fs::read(&path).unwrap(), b"changed by someone else");
    }

    #[test]
    fn a_second_write_needs_no_second_read() {
        // Arrange
        let rig = rig();
        rig.write.call(&put("f.txt", "one")).unwrap();

        // Act
        let out = rig.write.call(&put("f.txt", "two"));

        // Assert
        assert!(out.is_ok(), "{out:?}");
        assert_eq!(fs::read(rig.dir.path().join("f.txt")).unwrap(), b"two");
    }

    #[test]
    fn refuses_content_over_the_limit() {
        // Arrange
        let rig = rig();

        // Act
        let err = rig
            .write
            .call(&put("big.txt", &"x".repeat(LIMIT + 1)))
            .unwrap_err();

        // Assert
        assert!(matches!(err, ToolError::TooLarge { .. }), "{err}");
        assert!(!rig.dir.path().join("big.txt").exists());
    }

    #[test]
    fn refuses_a_path_outside_the_workspace() {
        // Arrange
        let rig = rig();

        // Act
        let parent = rig.write.call(&put("../escape.txt", "x")).unwrap_err();
        let absolute = rig.write.call(&put("/tmp/escape.txt", "x")).unwrap_err();

        // Assert
        assert!(matches!(
            parent,
            ToolError::Sandbox(SandboxError::ParentComponent(_))
        ));
        assert!(matches!(
            absolute,
            ToolError::Sandbox(SandboxError::Absolute(_))
        ));
    }

    #[test]
    fn refuses_to_write_through_a_symlink() {
        // Arrange
        let rig = rig();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("target.txt"), "keep").unwrap();
        symlink(
            outside.path().join("target.txt"),
            rig.dir.path().join("link.txt"),
        )
        .unwrap();
        symlink(outside.path(), rig.dir.path().join("linkdir")).unwrap();

        // Act
        let file = rig.write.call(&put("link.txt", "x")).unwrap_err();
        let nested = rig.write.call(&put("linkdir/new.txt", "x")).unwrap_err();

        // Assert
        assert!(matches!(file, ToolError::Sandbox(_)), "{file:?}");
        assert!(matches!(
            nested,
            ToolError::Sandbox(SandboxError::Symlink(_) | SandboxError::OutsideRoot(_))
        ));
        assert_eq!(
            fs::read(outside.path().join("target.txt")).unwrap(),
            b"keep"
        );
        assert!(!outside.path().join("new.txt").exists());
    }

    #[test]
    fn refuses_a_missing_parent_directory() {
        // Arrange
        let rig = rig();

        // Act
        let err = rig.write.call(&put("nope/new.txt", "x")).unwrap_err();

        // Assert
        assert!(matches!(err, ToolError::Sandbox(SandboxError::NotFound(_))));
    }
}
