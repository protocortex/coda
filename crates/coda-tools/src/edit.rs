// SPDX-License-Identifier: AGPL-3.0-only

//! The `edit` tool: replace one unique piece of text in a workspace file.

use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{Gid, Mode, Stat, Uid, fchmod, fchown, fstat};
use serde::Deserialize;
use serde_json::Value;

use crate::sandbox::{Sandbox, SandboxDir, SandboxError};
use crate::{Tool, ToolError, ToolOutput, parse_args};

const NAME: &str = "edit";

/// Default cap on the size of a file the tool will rewrite.
pub const DEFAULT_MAX_FILE_BYTES: usize = 1024 * 1024;

/// The longest file name common filesystems accept.
const NAME_MAX: usize = 255;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
}

/// Replaces exactly one occurrence of a string in a workspace file.
///
/// The rewrite goes to a temporary sibling that is renamed over the original,
/// so a reader never sees a half written file. If the file changed since it
/// was read, the edit is refused instead of overwriting the newer content.
pub struct EditTool {
    sandbox: Sandbox,
    max_file_bytes: usize,
    before_replace: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl EditTool {
    /// Creates the tool, refusing files larger than `max_file_bytes`.
    #[must_use]
    pub fn new(sandbox: Sandbox, max_file_bytes: usize) -> Self {
        Self {
            sandbox,
            max_file_bytes,
            before_replace: None,
        }
    }

    /// Runs `hook` after the file is read and before it is replaced, so tests
    /// can change the file at the exact moment a race would.
    #[cfg(test)]
    #[must_use]
    fn with_before_replace(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.before_replace = Some(Arc::new(hook));
        self
    }
}

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let args: EditArgs = parse_args(NAME, input)?;
        if args.old_string.is_empty() {
            return Err(ToolError::InvalidInput {
                tool: NAME,
                reason: "old_string must not be empty".into(),
            });
        }
        let io_err = |source| ToolError::Io {
            path: args.path.clone(),
            source,
        };
        let (dir, name) = self.sandbox.open_parent(Path::new(&args.path))?;
        let mut file = dir.open_file(&name)?;
        let stat = fstat(&file).map_err(|e| io_err(e.into()))?;
        let snapshot = file.metadata().map_err(io_err)?;

        let limit = u64::try_from(self.max_file_bytes).unwrap_or(u64::MAX);
        let mut bytes = Vec::new();
        (&mut file)
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(io_err)?;
        if bytes.len() > self.max_file_bytes {
            return Err(ToolError::TooLarge {
                path: args.path,
                limit: self.max_file_bytes,
            });
        }
        let text = String::from_utf8(bytes).map_err(|_| ToolError::NotUtf8 {
            path: args.path.clone(),
        })?;

        let updated = replace_unique(&args.path, &text, &args.old_string, &args.new_string)?;
        if let Some(hook) = &self.before_replace {
            hook();
        }
        let original = Original {
            file: &file,
            stat: &stat,
            snapshot: &snapshot,
        };
        replace_file(&dir, &name, &original, updated.as_bytes(), &args.path)?;
        Ok(ToolOutput {
            text: format!("edited {}", args.path),
            truncated: false,
        })
    }
}

/// The file as it was when the tool opened and read it.
struct Original<'a> {
    file: &'a File,
    stat: &'a Stat,
    snapshot: &'a Metadata,
}

/// How often `needle` occurs in `haystack`, counting overlapping matches,
/// and where the first one starts.
fn find_all(haystack: &str, needle: &str) -> (usize, Option<usize>) {
    let mut count = 0;
    let mut first = None;
    let mut from = 0;
    while let Some(offset) = haystack[from..].find(needle) {
        let at = from + offset;
        first.get_or_insert(at);
        count += 1;
        // Step one character, not one byte, so the slice stays on a boundary.
        from = at + haystack[at..].chars().next().map_or(1, char::len_utf8);
    }
    (count, first)
}

/// Swaps the single `old` for `new`.
///
/// The text is matched as sent first; only when it is absent from a file with
/// CRLF endings is it retried with CRLF, so LF-only lines of a mixed file
/// are edited as they are.
fn replace_unique(path: &str, text: &str, old: &str, new: &str) -> Result<String, ToolError> {
    let mut attempts = vec![(old.to_owned(), new.to_owned())];
    if text.contains("\r\n") {
        attempts.push((to_crlf(old), to_crlf(new)));
    }
    for (old, new) in &attempts {
        match find_all(text, old) {
            (0, _) => {}
            (1, Some(at)) => {
                let mut updated = String::with_capacity(text.len() - old.len() + new.len());
                updated.push_str(&text[..at]);
                updated.push_str(new);
                updated.push_str(&text[at + old.len()..]);
                return Ok(updated);
            }
            (count, _) => {
                return Err(ToolError::NotUnique {
                    path: path.to_owned(),
                    count,
                });
            }
        }
    }
    Err(ToolError::NoMatch {
        path: path.to_owned(),
    })
}

fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Writes `contents` to a temp file next to `name` and renames it into place,
/// unless `name` no longer is the file that was read.
fn replace_file(
    dir: &SandboxDir,
    name: &OsStr,
    original: &Original<'_>,
    contents: &[u8],
    display: &str,
) -> Result<(), ToolError> {
    let temp = temp_name(name);
    let written = write_temp(dir, &temp, original, contents)
        .map_err(ToolError::from)
        .and_then(|()| ensure_unchanged(dir, name, original.snapshot, display))
        .and_then(|()| dir.rename(&temp, name).map_err(ToolError::from));
    if written.is_err() {
        // Best effort: the write error is the one worth reporting.
        let _ = dir.remove(&temp);
    }
    written
}

/// Refuses when `name` is not the same file, with the same size and
/// modification time, as `snapshot`.
fn ensure_unchanged(
    dir: &SandboxDir,
    name: &OsStr,
    snapshot: &Metadata,
    display: &str,
) -> Result<(), ToolError> {
    let conflict = || ToolError::Conflict {
        path: display.to_owned(),
    };
    let current = dir
        .open_file(name)
        .and_then(|file| {
            file.metadata().map_err(|source| SandboxError::Io {
                path: display.to_owned(),
                source,
            })
        })
        .map_err(|_| conflict())?;
    let same = current.dev() == snapshot.dev()
        && current.ino() == snapshot.ino()
        && current.size() == snapshot.size()
        && current.mtime() == snapshot.mtime()
        && current.mtime_nsec() == snapshot.mtime_nsec();
    if same { Ok(()) } else { Err(conflict()) }
}

fn write_temp(
    dir: &SandboxDir,
    temp: &OsStr,
    original: &Original<'_>,
    contents: &[u8],
) -> Result<(), SandboxError> {
    let io_err = |source| SandboxError::Io {
        path: temp.to_string_lossy().into_owned(),
        source,
    };
    let mode = Mode::from_raw_mode(original.stat.st_mode);
    let mut file = dir.create_new(temp, mode)?;
    file.write_all(contents).map_err(io_err)?;
    // Ownership first, because changing it can clear the setuid bits that the
    // mode restore below puts back. Ignored when not permitted (EPERM).
    let _ = fchown(
        &file,
        Some(Uid::from_raw(original.stat.st_uid)),
        Some(Gid::from_raw(original.stat.st_gid)),
    );
    // The creation mode is filtered by the umask; restore the original bits.
    fchmod(&file, mode).map_err(|e| io_err(e.into()))?;
    copy_xattrs(original.file, &file);
    file.sync_all().map_err(io_err)
}

/// Copies extended attributes onto the replacement; losing one is not worth
/// failing the edit.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn copy_xattrs(from: &File, to: &File) {
    use rustix::fs::{XattrFlags, fgetxattr, flistxattr};

    const XATTR_BUF_BYTES: usize = 64 * 1024;
    let mut names = vec![0_u8; XATTR_BUF_BYTES];
    let Ok(len) = flistxattr(from, &mut names[..]) else {
        return;
    };
    let mut value = vec![0_u8; XATTR_BUF_BYTES];
    for name in names[..len].split(|b| *b == 0).filter(|n| !n.is_empty()) {
        if let Ok(size) = fgetxattr(from, name, &mut value[..]) {
            let _ = rustix::fs::fsetxattr(to, name, &value[..size], XattrFlags::empty());
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn copy_xattrs(_from: &File, _to: &File) {}

/// A hidden sibling name; the original is cut so the result fits `NAME_MAX`.
fn temp_name(name: &OsStr) -> OsString {
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let suffix = format!(".coda-edit-{}-{unique}", std::process::id());
    let room = NAME_MAX - 1 - suffix.len();
    let kept = &name.as_bytes()[..name.len().min(room)];
    let mut temp = vec![b'.'];
    temp.extend_from_slice(kept);
    temp.extend_from_slice(suffix.as_bytes());
    OsString::from_vec(temp)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::sandbox::SandboxError;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    const LIMIT: usize = 64;

    fn setup(name: &str, body: &[u8]) -> (TempDir, EditTool) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(name), body).unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        (dir, EditTool::new(sandbox, LIMIT))
    }

    fn edit(path: &str, old: &str, new: &str) -> Value {
        json!({"path": path, "old_string": old, "new_string": new})
    }

    #[test]
    fn replaces_a_unique_match() {
        // Arrange
        let (dir, tool) = setup("f.txt", b"alpha beta gamma");

        // Act
        let out = tool.call(&edit("f.txt", "beta", "BETA")).unwrap();

        // Assert
        assert!(!out.truncated);
        assert_eq!(
            fs::read(dir.path().join("f.txt")).unwrap(),
            b"alpha BETA gamma"
        );
    }

    #[test]
    fn zero_matches_is_rejected_and_file_untouched() {
        // Arrange
        let (dir, tool) = setup("f.txt", b"alpha");

        // Act
        let result = tool.call(&edit("f.txt", "zeta", "x"));

        // Assert
        assert!(matches!(result, Err(ToolError::NoMatch { .. })));
        assert_eq!(fs::read(dir.path().join("f.txt")).unwrap(), b"alpha");
    }

    #[test]
    fn non_unique_match_is_rejected_with_count() {
        // Arrange
        let (dir, tool) = setup("f.txt", b"ab ab ab");

        // Act
        let result = tool.call(&edit("f.txt", "ab", "x"));

        // Assert
        assert!(matches!(result, Err(ToolError::NotUnique { count: 3, .. })));
        assert_eq!(fs::read(dir.path().join("f.txt")).unwrap(), b"ab ab ab");
    }

    #[test]
    fn empty_old_string_is_invalid_input() {
        // Arrange
        let (_dir, tool) = setup("f.txt", b"alpha");

        // Act
        let result = tool.call(&edit("f.txt", "", "x"));

        // Assert
        assert!(matches!(result, Err(ToolError::InvalidInput { .. })));
    }

    #[test]
    fn crlf_file_keeps_crlf_when_model_sends_lf() {
        // Arrange
        let (dir, tool) = setup("f.txt", b"a\r\nb\r\nc\r\n");

        // Act
        tool.call(&edit("f.txt", "b\nc", "x\ny")).unwrap();

        // Assert
        assert_eq!(
            fs::read(dir.path().join("f.txt")).unwrap(),
            b"a\r\nx\r\ny\r\n"
        );
    }

    #[test]
    fn lf_file_stays_lf() {
        // Arrange
        let (dir, tool) = setup("f.txt", b"a\nb\nc\n");

        // Act
        tool.call(&edit("f.txt", "b\n", "x\ny\n")).unwrap();

        // Assert
        assert_eq!(fs::read(dir.path().join("f.txt")).unwrap(), b"a\nx\ny\nc\n");
    }

    #[test]
    fn file_mode_is_preserved() {
        // Arrange
        let (dir, tool) = setup("run.sh", b"echo one");
        let path = dir.path().join("run.sh");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();

        // Act
        tool.call(&edit("run.sh", "one", "two")).unwrap();

        // Assert
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o750);
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        // Arrange
        let (dir, tool) = setup("f.txt", b"one");

        // Act
        tool.call(&edit("f.txt", "one", "two")).unwrap();

        // Assert
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("f.txt")]);
    }

    #[test]
    fn oversized_file_is_rejected() {
        // Arrange
        let (_dir, tool) = setup("f.txt", &[b'a'; LIMIT + 1]);

        // Act
        let result = tool.call(&edit("f.txt", "a", "b"));

        // Assert
        assert!(matches!(
            result,
            Err(ToolError::TooLarge { limit: LIMIT, .. })
        ));
    }

    #[test]
    fn non_utf8_file_is_rejected() {
        // Arrange
        let (_dir, tool) = setup("f.bin", &[0xff, 0xfe, b'a']);

        // Act
        let result = tool.call(&edit("f.bin", "a", "b"));

        // Assert
        assert!(matches!(result, Err(ToolError::NotUtf8 { .. })));
    }

    #[test]
    fn symlinked_target_is_refused() {
        // Arrange
        let (dir, tool) = setup("real.txt", b"one");
        symlink("real.txt", dir.path().join("link.txt")).unwrap();

        // Act
        let result = tool.call(&edit("link.txt", "one", "two"));

        // Assert
        assert!(matches!(
            result,
            Err(ToolError::Sandbox(SandboxError::Symlink(_)))
        ));
        assert_eq!(fs::read(dir.path().join("real.txt")).unwrap(), b"one");
    }

    #[test]
    fn parent_traversal_is_refused() {
        // Arrange
        let (_dir, tool) = setup("f.txt", b"one");

        // Act
        let result = tool.call(&edit("../f.txt", "one", "two"));

        // Assert
        assert!(matches!(
            result,
            Err(ToolError::Sandbox(SandboxError::ParentComponent(_)))
        ));
    }

    #[test]
    fn edits_a_file_in_a_subdirectory() {
        // Arrange
        let (dir, tool) = setup("f.txt", b"x");
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/g.txt"), "one").unwrap();

        // Act
        tool.call(&edit("sub/g.txt", "one", "two")).unwrap();

        // Assert
        assert_eq!(fs::read(dir.path().join("sub/g.txt")).unwrap(), b"two");
    }

    #[test]
    fn mixed_endings_edit_lf_only_lines_without_adding_cr() {
        // Arrange
        let (dir, tool) = setup("f.txt", b"a\nb\nc\r\nd\r\n");

        // Act
        tool.call(&edit("f.txt", "a\nb", "x\ny")).unwrap();

        // Assert
        assert_eq!(
            fs::read(dir.path().join("f.txt")).unwrap(),
            b"x\ny\nc\r\nd\r\n"
        );
    }

    #[test]
    fn overlapping_matches_are_ambiguous() {
        // Arrange
        let (dir, tool) = setup("f.txt", b"aaa");

        // Act
        let result = tool.call(&edit("f.txt", "aa", "x"));

        // Assert
        assert!(matches!(result, Err(ToolError::NotUnique { count: 2, .. })));
        assert_eq!(fs::read(dir.path().join("f.txt")).unwrap(), b"aaa");
    }

    #[test]
    fn file_replaced_between_read_and_write_is_a_conflict() {
        // Arrange
        let (dir, tool) = setup("f.txt", b"one");
        let root = dir.path().to_path_buf();
        let tool = tool.with_before_replace(move || {
            fs::write(root.join("other"), "theirs").unwrap();
            fs::rename(root.join("other"), root.join("f.txt")).unwrap();
        });

        // Act
        let result = tool.call(&edit("f.txt", "one", "two"));

        // Assert
        assert!(matches!(result, Err(ToolError::Conflict { .. })));
        assert_eq!(fs::read(dir.path().join("f.txt")).unwrap(), b"theirs");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn file_modified_in_place_between_read_and_write_is_a_conflict() {
        // Arrange
        let (dir, tool) = setup("f.txt", b"one");
        let path = dir.path().join("f.txt");
        let tool = tool.with_before_replace(move || fs::write(&path, "changed!").unwrap());

        // Act
        let result = tool.call(&edit("f.txt", "one", "two"));

        // Assert
        assert!(matches!(result, Err(ToolError::Conflict { .. })));
        assert_eq!(fs::read(dir.path().join("f.txt")).unwrap(), b"changed!");
    }

    #[test]
    fn file_with_a_maximum_length_name_is_editable() {
        // Arrange
        let name = "a".repeat(255);
        let (dir, tool) = setup(&name, b"one");

        // Act
        tool.call(&edit(&name, "one", "two")).unwrap();

        // Assert
        assert_eq!(fs::read(dir.path().join(&name)).unwrap(), b"two");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
