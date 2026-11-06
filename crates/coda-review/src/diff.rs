// SPDX-License-Identifier: Apache-2.0

//! Reading `git diff` output. The text is untrusted: the parser never indexes past the
//! end of its input and treats anything it does not understand as plain text.

use std::path::Path;
use std::process::Command;

/// Lines of context git adds around each change.
pub const CONTEXT_LINES: u32 = 3;

const DIFF_HEADER: &str = "diff --git ";
const OLD_PREFIX: &str = "a/";
const NEW_PREFIX: &str = "b/";
const DEV_NULL: &str = "/dev/null";
const NEW_FILE: &str = "new file mode";
const DELETED_FILE: &str = "deleted file mode";
const RENAME_FROM: &str = "rename from ";
const RENAME_TO: &str = "rename to ";
const BINARY_FILES: &str = "Binary files ";
const BINARY_PATCH: &str = "GIT binary patch";
const OLD_MARK: &str = "--- ";
const NEW_MARK: &str = "+++ ";
const HUNK_MARK: &str = "@@ -";
const HUNK_END: &str = " @@";
const NO_NEWLINE: char = '\\';
const PATH_SPLIT_EXTRA: usize = 5;
const NUL: char = '\0';
const STAGED_FLAG: &str = "--cached";
const QUOTE: char = '"';
const QUOTED_NEW_START: &str = " \"b/";
const OCTAL_DIGITS: u32 = 3;
const OCTAL_BASE: u32 = 8;
const BYTE_MASK: u32 = 0xff;
const BAD_BYTE: u8 = b'?';
const BELL: u8 = 0x07;
const BACKSPACE: u8 = 0x08;
const VERTICAL_TAB: u8 = 0x0b;
const FORM_FEED: u8 = 0x0c;

/// What happened to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The file is new.
    Added,
    /// The file changed in place.
    Modified,
    /// The file was removed.
    Deleted,
    /// The file moved, with or without edits.
    Renamed,
}

/// The kind of one line in a hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Present on both sides.
    Context,
    /// Present only on the new side.
    Added,
    /// Present only on the old side.
    Removed,
}

/// One line of a hunk, without its marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Kind of line.
    pub kind: LineKind,
    /// Text of the line.
    pub text: String,
}

/// One hunk of a file diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// First old-side line.
    pub old_start: u32,
    /// Number of old-side lines.
    pub old_len: u32,
    /// First new-side line.
    pub new_start: u32,
    /// Number of new-side lines.
    pub new_len: u32,
    /// The lines.
    pub lines: Vec<Line>,
}

impl Hunk {
    /// True when the hunk adds at least one line.
    #[must_use]
    pub fn has_additions(&self) -> bool {
        self.lines.iter().any(|l| l.kind == LineKind::Added)
    }
}

/// The diff of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Path on the new side (the old path for a deleted file).
    pub path: String,
    /// Path before a rename.
    pub old_path: Option<String>,
    /// What happened to the file.
    pub status: Status,
    /// True for a binary file, which has no hunks.
    pub binary: bool,
    /// The hunks.
    pub hunks: Vec<Hunk>,
}

/// Parses the output of `git diff` made with the `a/` and `b/` prefixes.
#[must_use]
pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut current: Option<FileDiff> = None;
    let mut open: Option<(Hunk, u32, u32)> = None;
    for line in text.lines() {
        if line.starts_with(NO_NEWLINE) {
            continue;
        }
        if let Some((hunk, old_left, new_left)) = open.as_mut()
            && (*old_left > 0 || *new_left > 0)
            && let Some((kind, body)) = split_marker(line)
        {
            if kind != LineKind::Added {
                *old_left = old_left.saturating_sub(1);
            }
            if kind != LineKind::Removed {
                *new_left = new_left.saturating_sub(1);
            }
            hunk.lines.push(Line {
                kind,
                text: body.to_owned(),
            });
            continue;
        }
        handle_line(&mut current, &mut open, line, &mut files);
    }
    close_hunk(&mut current, &mut open);
    files.extend(current);
    files
}

fn split_marker(line: &str) -> Option<(LineKind, &str)> {
    match line.as_bytes().first() {
        Some(b'+') => Some((LineKind::Added, &line[1..])),
        Some(b'-') => Some((LineKind::Removed, &line[1..])),
        Some(b' ') => Some((LineKind::Context, &line[1..])),
        None => Some((LineKind::Context, line)),
        Some(_) => None,
    }
}

fn close_hunk(current: &mut Option<FileDiff>, open: &mut Option<(Hunk, u32, u32)>) {
    if let (Some(file), Some((hunk, _, _))) = (current.as_mut(), open.take()) {
        file.hunks.push(hunk);
    }
}

fn handle_line(
    current: &mut Option<FileDiff>,
    open: &mut Option<(Hunk, u32, u32)>,
    line: &str,
    files: &mut Vec<FileDiff>,
) {
    close_hunk(current, open);
    if let Some(rest) = line.strip_prefix(DIFF_HEADER) {
        files.extend(current.take());
        *current = Some(FileDiff {
            path: guess_path(rest),
            old_path: None,
            status: Status::Modified,
            binary: false,
            hunks: Vec::new(),
        });
        return;
    }
    let Some(file) = current.as_mut() else {
        return;
    };
    if line.starts_with(NEW_FILE) {
        file.status = Status::Added;
    } else if line.starts_with(DELETED_FILE) {
        file.status = Status::Deleted;
    } else if let Some(from) = line.strip_prefix(RENAME_FROM) {
        file.status = Status::Renamed;
        file.old_path = Some(unquote(from));
    } else if let Some(to) = line.strip_prefix(RENAME_TO) {
        file.path = unquote(to);
    } else if line.starts_with(BINARY_FILES) || line.starts_with(BINARY_PATCH) {
        file.binary = true;
    } else if let Some(path) = line.strip_prefix(NEW_MARK) {
        if let Some(path) = side_path(path, NEW_PREFIX) {
            file.path = path;
        }
    } else if let Some(path) = line.strip_prefix(OLD_MARK) {
        if file.status == Status::Deleted
            && let Some(path) = side_path(path, OLD_PREFIX)
        {
            file.path = path;
        }
    } else if let Some((hunk, old_left, new_left)) = parse_hunk_header(line) {
        *open = Some((hunk, old_left, new_left));
    }
}

fn side_path(raw: &str, prefix: &str) -> Option<String> {
    let raw = raw.split('\t').next().unwrap_or(raw);
    if raw == DEV_NULL {
        return None;
    }
    let unquoted = unquote(raw);
    Some(
        unquoted
            .strip_prefix(prefix)
            .map_or(unquoted.clone(), str::to_owned),
    )
}

/// The new-side path of a `diff --git` header, which has no separator between the two
/// names. Git quotes a name that holds a quote, backslash or control character.
fn guess_path(rest: &str) -> String {
    let rest = rest.trim_end();
    if rest.starts_with(QUOTE) {
        if let Some((_, after)) = read_quoted(rest) {
            let second = after.trim_start_matches(' ');
            return strip_prefix_or_keep(&unquote(second), NEW_PREFIX);
        }
    } else if let Some((_, second)) = rest.rsplit_once(QUOTED_NEW_START) {
        return strip_prefix_or_keep(
            &unquote(&format!("{QUOTE}{NEW_PREFIX}{second}")),
            NEW_PREFIX,
        );
    }
    let both = rest.len().saturating_sub(PATH_SPLIT_EXTRA);
    if both.is_multiple_of(2)
        && let Some(half) = rest.get(OLD_PREFIX.len()..OLD_PREFIX.len() + both / 2)
        && rest.starts_with(OLD_PREFIX)
    {
        return unquote(half);
    }
    let after = rest.rsplit_once(" b/").map_or(rest, |(_, p)| p);
    unquote(after)
}

fn strip_prefix_or_keep(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix).unwrap_or(path).to_owned()
}

/// Reads one quoted name at the start of `text`. Returns the decoded name and what follows
/// the closing quote.
fn read_quoted(text: &str) -> Option<(String, &str)> {
    let body = text.strip_prefix(QUOTE)?;
    let bytes = body.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'"' => return Some((decode_escapes(&body[..at]), &body[at + 1..])),
            _ => at += 1,
        }
    }
    None
}

fn decode_escapes(inner: &str) -> String {
    let bytes = inner.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        at += 1;
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        let Some(&next) = bytes.get(at) else {
            out.push(b'\\');
            break;
        };
        at += 1;
        match next {
            b'a' => out.push(BELL),
            b'b' => out.push(BACKSPACE),
            b'f' => out.push(FORM_FEED),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(VERTICAL_TAB),
            b'0'..=b'7' => {
                let mut value = u32::from(next - b'0');
                let mut digits = 1;
                while digits < OCTAL_DIGITS
                    && let Some(&d @ b'0'..=b'7') = bytes.get(at)
                {
                    value = value * OCTAL_BASE + u32::from(d - b'0');
                    at += 1;
                    digits += 1;
                }
                out.push(u8::try_from(value & BYTE_MASK).unwrap_or(BAD_BYTE));
            }
            other => out.push(other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn unquote(path: &str) -> String {
    match read_quoted(path) {
        Some((decoded, "")) => decoded,
        _ => path.to_owned(),
    }
}

fn parse_range(text: &str) -> Option<(u32, u32)> {
    match text.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((text.parse().ok()?, 1)),
    }
}

fn parse_hunk_header(line: &str) -> Option<(Hunk, u32, u32)> {
    let rest = line.strip_prefix(HUNK_MARK)?;
    let (old, rest) = rest.split_once(" +")?;
    let new = rest.split(HUNK_END).next()?;
    let (old_start, old_len) = parse_range(old)?;
    let (new_start, new_len) = parse_range(new)?;
    let hunk = Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        lines: Vec::new(),
    };
    Some((hunk, old_len, new_len))
}

/// Why running git failed.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The range looks like an option.
    #[error("revision {0:?} is not allowed")]
    BadRevision(String),
    /// Git could not run or exited with an error.
    #[error("git failed: {0}")]
    Failed(String),
}

fn run_git(repo_dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .map_err(|e| GitError::Failed(e.to_string()))?;
    if !out.status.success() {
        return Err(GitError::Failed(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs `git diff base...head` in `repo_dir`, the change the head branch brings against
/// the merge base. External diff programs and text conversion filters are off.
///
/// # Errors
/// Returns [`GitError`] for a revision that looks like an option or a git failure.
pub fn git_diff(repo_dir: &Path, base: &str, head: &str) -> Result<String, GitError> {
    for rev in [base, head] {
        if rev.is_empty() || rev.starts_with('-') || rev.contains("..") {
            return Err(GitError::BadRevision(rev.to_owned()));
        }
    }
    run_diff(repo_dir, &format!("{base}...{head}"))
}

/// Runs `git diff --cached` in `repo_dir`, the changes staged for the next commit.
///
/// # Errors
/// Returns [`GitError`] when git fails.
pub fn git_diff_staged(repo_dir: &Path) -> Result<String, GitError> {
    run_diff(repo_dir, STAGED_FLAG)
}

fn run_diff(repo_dir: &Path, selection: &str) -> Result<String, GitError> {
    let unified = format!("-U{CONTEXT_LINES}");
    run_git(
        repo_dir,
        &[
            "-c",
            "core.quotepath=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            &unified,
            selection,
            "--",
        ],
    )
}

/// Lists the files git tracks in `repo_dir`.
///
/// # Errors
/// Returns [`GitError`] when git fails.
pub fn tracked_files(repo_dir: &Path) -> Result<Vec<String>, GitError> {
    let text = run_git(repo_dir, &["ls-files", "-z"])?;
    Ok(text
        .split(NUL)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_modified_file_parses_with_line_kinds_and_numbers() {
        // Arrange
        let text = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@ fn main
 one
-two
+TWO
+two and a half
 three
";

        // Act
        let files = parse_diff(text);

        // Assert
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.path, "src/a.rs");
        assert_eq!(file.status, Status::Modified);
        let hunk = &file.hunks[0];
        assert_eq!((hunk.old_start, hunk.old_len), (1, 3));
        assert_eq!((hunk.new_start, hunk.new_len), (1, 4));
        let kinds: Vec<_> = hunk.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Added,
                LineKind::Context
            ]
        );
    }

    #[test]
    fn added_deleted_renamed_and_binary_files_are_told_apart() {
        // Arrange
        let text = "\
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hi
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/old name.txt b/new name.txt
similarity index 90%
rename from old name.txt
rename to new name.txt
diff --git a/img.png b/img.png
Binary files a/img.png and b/img.png differ
";

        // Act
        let files = parse_diff(text);

        // Assert
        let summary: Vec<_> = files
            .iter()
            .map(|f| (f.path.as_str(), f.status, f.binary, f.hunks.len()))
            .collect();
        assert_eq!(
            summary,
            [
                ("new.txt", Status::Added, false, 1),
                ("gone.txt", Status::Deleted, false, 1),
                ("new name.txt", Status::Renamed, false, 0),
                ("img.png", Status::Modified, true, 0),
            ]
        );
        assert_eq!(files[2].old_path.as_deref(), Some("old name.txt"));
    }

    #[test]
    fn content_that_looks_like_a_header_stays_inside_its_hunk() {
        // Arrange
        let text = "\
diff --git a/a.md b/a.md
--- a/a.md
+++ b/a.md
@@ -1,2 +1,2 @@
--- a/evil
+++ b/evil
 keep
+diff --git a/x b/x
";

        // Act
        let files = parse_diff(text);

        // Assert
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.md");
        let texts: Vec<_> = files[0].hunks[0]
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(texts, ["-- a/evil", "++ b/evil", "keep"]);
    }

    #[test]
    fn a_no_newline_marker_is_skipped() {
        // Arrange
        let text = "\
diff --git a/a b/a
--- a/a
+++ b/a
@@ -1 +1 @@
-x
\\ No newline at end of file
+y
\\ No newline at end of file
";

        // Act
        let files = parse_diff(text);

        // Assert
        assert_eq!(files[0].hunks[0].lines.len(), 2);
    }

    #[test]
    fn garbage_and_truncated_input_never_panics() {
        // Arrange
        let cases = [
            "",
            "not a diff",
            "@@ -1 +1 @@\n+x\n",
            "diff --git a/a b/a\n@@ -x,y +z @@\n+q\n",
            "diff --git a/a b/a\n@@ -1,9 +1,9 @@\n+only one\n",
            "diff --git \n",
            "diff --git a/\u{e9} b/\u{e9}\n--- a/\u{e9}\n+++ b/\u{e9}\n@@ -1 +1 @@\n-\u{e9}\n+\u{e8}\n",
        ];

        for text in cases {
            // Act
            let files = parse_diff(text);

            // Assert
            assert!(files.len() <= 1, "{text:?}");
        }
    }

    #[test]
    fn a_quoted_path_is_unquoted() {
        // Arrange
        let text = "diff --git \"a/q\\\"x\" \"b/q\\\"x\"\n--- \"a/q\\\"x\"\n+++ \"b/q\\\"x\"\n@@ -1 +1 @@\n-a\n+b\n";

        // Act
        let files = parse_diff(text);

        // Assert
        assert_eq!(files[0].path, "q\"x");
    }

    #[test]
    fn a_quoted_header_without_a_content_line_still_gives_the_real_path() {
        // Arrange
        let cases = [
            (
                "diff --git \"a/x y\\t\" \"b/x y\\t\"\nold mode 100644\nnew mode 100755\n",
                "x y\t",
            ),
            (
                "diff --git \"a/\\001x\" \"b/\\001x\"\nBinary files a/x and b/x differ\n",
                "\u{1}x",
            ),
            (
                "diff --git \"a/caf\\303\\251\" \"b/caf\\303\\251\"\nold mode 100644\nnew mode 100755\n",
                "caf\u{e9}",
            ),
            (
                "diff --git \"a/q\\\"x\" \"b/q\\\"x\"\nold mode 100644\nnew mode 100755\n",
                "q\"x",
            ),
            (
                "diff --git \"a/a\\\\b\" \"b/a\\\\b\"\nold mode 100644\nnew mode 100755\n",
                "a\\b",
            ),
            (
                "diff --git \"a/\\a\\b\\f\\r\\v\" \"b/\\a\\b\\f\\r\\v\"\nold mode 100644\nnew mode 100755\n",
                "\u{7}\u{8}\u{c}\r\u{b}",
            ),
        ];

        for (text, expected) in cases {
            // Act
            let files = parse_diff(text);

            // Assert
            assert_eq!(files.len(), 1, "{text:?}");
            assert_eq!(files[0].path, expected, "{text:?}");
        }
    }

    #[test]
    fn a_quoted_pure_rename_keeps_both_real_paths() {
        // Arrange
        let text = "diff --git \"a/old\\tname\" \"b/new\\tname\"\nsimilarity index 100%\nrename from \"old\\tname\"\nrename to \"new\\tname\"\n";

        // Act
        let files = parse_diff(text);

        // Assert
        assert_eq!(files[0].path, "new\tname");
        assert_eq!(files[0].old_path.as_deref(), Some("old\tname"));
    }

    #[test]
    fn an_unquoted_old_name_with_a_quoted_new_name_gives_the_new_name() {
        // Arrange
        let text = "diff --git a/old \"b/new\\tname\"\nold mode 100644\nnew mode 100755\n";

        // Act
        let files = parse_diff(text);

        // Assert
        assert_eq!(files[0].path, "new\tname");
    }

    #[test]
    fn revisions_that_look_like_options_are_refused() {
        // Arrange
        let dir = Path::new(".");

        // Act and Assert
        for (base, head) in [
            ("--output=x", "HEAD"),
            ("main", "-p"),
            ("a..b", "c"),
            ("", "c"),
        ] {
            let err = git_diff(dir, base, head).unwrap_err();
            assert!(matches!(err, GitError::BadRevision(_)), "{base} {head}");
        }
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    }

    #[test]
    fn the_staged_diff_holds_staged_changes_and_not_unstaged_ones() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git_in(p, &["init", "-q", "-b", "main"]);
        std::fs::write(p.join("a.txt"), "one\n").unwrap();
        std::fs::write(p.join("b.txt"), "one\n").unwrap();
        git_in(p, &["add", "."]);
        git_in(p, &["commit", "-q", "-m", "base"]);
        std::fs::write(p.join("a.txt"), "one\ntwo\n").unwrap();
        git_in(p, &["add", "a.txt"]);
        std::fs::write(p.join("b.txt"), "one\nthree\n").unwrap();

        // Act
        let text = git_diff_staged(p).unwrap();

        // Assert
        let files = parse_diff(&text);
        assert_eq!(files.len(), 1, "{text}");
        assert_eq!(files[0].path, "a.txt");
    }
}
