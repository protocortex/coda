// SPDX-License-Identifier: Apache-2.0

//! The files one turn changed, read from the checkpoints around it.
//!
//! Both checkpoints are commits whose parent is `HEAD` at the time, so the
//! difference between them is the whole change of the working tree, including
//! what a `git pull`, merge or rebase during the turn brought in. A file counts
//! as the turn's own unless it matches the change of `HEAD` itself, that is,
//! the working tree held the old `HEAD` version before and holds the new one
//! after. Only plumbing that reads objects runs, so the user's index, `HEAD`
//! and branches are never touched.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use coda_protocol::methods::DiffFile;

use crate::checkpoint::{CheckpointError, Git};

const DIFF_LIMIT: Duration = Duration::from_secs(30);
/// Files listed for one turn.
pub(crate) const MAX_FILES: usize = 1000;
/// Changed lines above which the diff text is not built at all.
const MAX_PATCH_LINES: u64 = 20_000;
/// The longest diff text returned, cut at a line end.
pub(crate) const MAX_PATCH_BYTES: usize = 512 * 1024;
/// Names a path from the top of the repository, as written and not as a pattern.
const PATH_MAGIC: &str = ":(top,literal)";
const RAW_FIELDS: usize = 5;
const OLD_OBJECT_FIELD: usize = 2;
const NEW_OBJECT_FIELD: usize = 3;
const NUMSTAT_FIELDS: usize = 3;
const BINARY_COUNT: &str = "-";

/// What `session.diff` reports for one turn.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TurnDiff {
    pub files: Vec<DiffFile>,
    pub patch: String,
    pub truncated: bool,
}

/// One path whose object differs between two trees.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    path: String,
    old: String,
    new: String,
}

/// The change the turn made between the checkpoints `before` and `after`, in
/// the repository holding `cwd`; with `patch`, also its unified diff.
pub(crate) fn turn_diff(
    cwd: &Path,
    before: &str,
    after: &str,
    patch: bool,
) -> Result<TurnDiff, CheckpointError> {
    let git = Git::new(cwd, Instant::now() + DIFF_LIMIT);
    let mut own = changes(&git, before, after)?;
    if let (Some(from), Some(to)) = (parent(&git, before), parent(&git, after))
        && from != to
    {
        let brought: HashMap<String, (String, String)> = changes(&git, &from, &to)?
            .into_iter()
            .map(|change| (change.path, (change.old, change.new)))
            .collect();
        let committed = committed_during_turn(&git, before, &from, &to)?;
        own.retain(|change| {
            committed.contains(&change.path)
                || brought.get(&change.path) != Some(&(change.old.clone(), change.new.clone()))
        });
    }
    let mut truncated = own.len() > MAX_FILES;
    own.truncate(MAX_FILES);
    if own.is_empty() {
        return Ok(TurnDiff::default());
    }
    let specs: Vec<String> = own
        .iter()
        .map(|change| format!("{PATH_MAGIC}{}", change.path))
        .collect();
    let paths: Vec<&str> = specs.iter().map(String::as_str).collect();
    let files = numstat(&git, before, after, &paths)?;
    let mut text = String::new();
    if patch {
        let lines: u64 = files.iter().map(|file| file.added + file.deleted).sum();
        if lines > MAX_PATCH_LINES {
            truncated = true;
        } else {
            let (shown, cut) = cut_at_line(unified(&git, before, after, &paths)?);
            text = shown;
            truncated |= cut;
        }
    }
    Ok(TurnDiff {
        files,
        patch: text,
        truncated,
    })
}

/// The paths changed by commits on the way from `from` to `to` that were
/// written after the checkpoint `before` was taken, so a commit the agent
/// made is the turn's change and not something `HEAD` brought in. The author
/// date decides, since a pull or rebase keeps the date its commits were first
/// written with. Merge commits are left out.
fn committed_during_turn(
    git: &Git<'_>,
    before: &str,
    from: &str,
    to: &str,
) -> Result<HashSet<String>, CheckpointError> {
    let started: i64 = git
        .git(None, &["show", "-s", "--format=%ct", before])?
        .trim()
        .parse()
        .unwrap_or(i64::MAX);
    let log = git.git(
        None,
        &[
            "log",
            "--no-merges",
            "--no-renames",
            "--name-only",
            "-z",
            "--format=%x01%at",
            &format!("{from}..{to}"),
        ],
    )?;
    let mut paths = HashSet::new();
    for commit in log.split('\u{1}').filter(|commit| !commit.is_empty()) {
        let digits = commit
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(commit.len());
        let at: i64 = commit[..digits].parse().unwrap_or(0);
        if at >= started {
            let names = commit[digits..].trim_start_matches(['\n', '\0']);
            paths.extend(
                names
                    .split('\0')
                    .filter(|path| !path.is_empty())
                    .map(str::to_owned),
            );
        }
    }
    Ok(paths)
}

fn parent(git: &Git<'_>, commit: &str) -> Option<String> {
    git.git(
        None,
        &["rev-parse", "--verify", "--quiet", &format!("{commit}^")],
    )
    .ok()
    .map(|out| out.trim().to_owned())
}

/// The paths whose object differs between the trees of `from` and `to`.
fn changes(git: &Git<'_>, from: &str, to: &str) -> Result<Vec<Change>, CheckpointError> {
    let raw = git.git(
        None,
        &[
            "diff-tree",
            "-r",
            "--raw",
            "-z",
            "--no-abbrev",
            "--no-renames",
            from,
            to,
        ],
    )?;
    let mut found = Vec::new();
    let mut entries = raw.split('\0');
    while let Some(entry) = entries.next() {
        let fields: Vec<&str> = entry.split(' ').collect();
        let Some(path) = entries.next() else { break };
        if fields.len() == RAW_FIELDS {
            found.push(Change {
                path: path.to_owned(),
                old: fields[OLD_OBJECT_FIELD].to_owned(),
                new: fields[NEW_OBJECT_FIELD].to_owned(),
            });
        }
    }
    Ok(found)
}

/// Arguments of a `git diff` between the checkpoints that reads no user
/// configuration that could run a program, and takes `paths` as plain names.
fn diff_args<'a>(
    before: &'a str,
    after: &'a str,
    paths: &[&'a str],
    extra: &[&'a str],
) -> Vec<&'a str> {
    let mut args = vec![
        "diff",
        "--no-relative",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ];
    args.extend_from_slice(extra);
    args.extend([before, after, "--"]);
    args.extend_from_slice(paths);
    args
}

fn numstat(
    git: &Git<'_>,
    before: &str,
    after: &str,
    paths: &[&str],
) -> Result<Vec<DiffFile>, CheckpointError> {
    let out = git.git(None, &diff_args(before, after, paths, &["--numstat", "-z"]))?;
    let mut files = Vec::new();
    for entry in out.split('\0').filter(|entry| !entry.is_empty()) {
        let fields: Vec<&str> = entry.splitn(NUMSTAT_FIELDS, '\t').collect();
        if let [added, deleted, path] = fields[..] {
            let binary = added == BINARY_COUNT;
            files.push(DiffFile {
                path: path.to_owned(),
                added: added.parse().unwrap_or(0),
                deleted: deleted.parse().unwrap_or(0),
                binary,
            });
        }
    }
    Ok(files)
}

fn unified(
    git: &Git<'_>,
    before: &str,
    after: &str,
    paths: &[&str],
) -> Result<String, CheckpointError> {
    git.git(None, &diff_args(before, after, paths, &[]))
}

/// `text` cut to [`MAX_PATCH_BYTES`] at the last line end, and whether it was cut.
fn cut_at_line(text: String) -> (String, bool) {
    if text.len() <= MAX_PATCH_BYTES {
        return (text, false);
    }
    let mut end = MAX_PATCH_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let end = text[..end].rfind('\n').map_or(end, |at| at + 1);
    (text[..end].to_owned(), true)
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::checkpoint::snapshot;

    const LONG_AGO: &str = "2020-01-01T00:00:00Z";

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_DATE", LONG_AGO)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Commits as the turn would, with the author date of now.
    fn commit_now(dir: &Path, message: &str) {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(["commit", "-q", "-am", message])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        fs::write(dir.path().join("b.txt"), "keep\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "first"]);
        dir
    }

    fn mark(dir: &Path, label: &str) -> String {
        let reference = format!("refs/coda/checkpoints/1/{label}");
        snapshot(dir, &reference).unwrap();
        reference
    }

    fn paths(diff: &TurnDiff) -> Vec<&str> {
        diff.files.iter().map(|file| file.path.as_str()).collect()
    }

    #[test]
    fn a_turn_reports_the_files_it_edited_added_and_removed_with_their_line_counts() {
        // Arrange
        let dir = repo();
        let before = mark(dir.path(), "base");
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(dir.path().join("new.txt"), "x\n").unwrap();
        fs::remove_file(dir.path().join("b.txt")).unwrap();
        let after = mark(dir.path(), "1");

        // Act
        let diff = turn_diff(dir.path(), &before, &after, false).unwrap();

        // Assert
        assert_eq!(paths(&diff), ["a.txt", "b.txt", "new.txt"]);
        let counts: Vec<(u64, u64)> = diff.files.iter().map(|f| (f.added, f.deleted)).collect();
        assert_eq!(counts, [(2, 0), (0, 1), (1, 0)]);
        assert_eq!(diff.patch, "");
        assert!(!diff.truncated);
    }

    #[test]
    fn a_turn_that_changed_nothing_reports_no_files() {
        // Arrange
        let dir = repo();
        let before = mark(dir.path(), "base");
        let after = mark(dir.path(), "1");

        // Act
        let diff = turn_diff(dir.path(), &before, &after, true).unwrap();

        // Assert
        assert_eq!(diff, TurnDiff::default());
    }

    #[test]
    fn the_patch_is_a_unified_diff_of_only_the_turns_files() {
        // Arrange
        let dir = repo();
        let before = mark(dir.path(), "base");
        fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let after = mark(dir.path(), "1");

        // Act
        let diff = turn_diff(dir.path(), &before, &after, true).unwrap();

        // Assert
        assert!(
            diff.patch.starts_with("diff --git a/a.txt b/a.txt\n"),
            "{}",
            diff.patch
        );
        assert!(diff.patch.contains("-one\n+two\n"), "{}", diff.patch);
        assert!(!diff.patch.contains("b.txt"));
    }

    #[test]
    fn a_binary_file_is_listed_without_line_counts() {
        // Arrange
        let dir = repo();
        let before = mark(dir.path(), "base");
        fs::write(dir.path().join("logo.bin"), [0u8, 159, 146, 150, 0, 1]).unwrap();
        let after = mark(dir.path(), "1");

        // Act
        let diff = turn_diff(dir.path(), &before, &after, false).unwrap();

        // Assert
        assert_eq!(paths(&diff), ["logo.bin"]);
        assert!(diff.files[0].binary);
        assert_eq!((diff.files[0].added, diff.files[0].deleted), (0, 0));
    }

    #[test]
    fn a_file_a_pull_brought_in_during_the_turn_is_not_the_turns_change() {
        // Arrange
        let dir = repo();
        git(dir.path(), &["checkout", "-q", "-b", "upstream"]);
        fs::write(dir.path().join("pulled.txt"), "from upstream\n").unwrap();
        fs::write(dir.path().join("b.txt"), "upstream edit\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "upstream"]);
        git(dir.path(), &["checkout", "-q", "main"]);
        let before = mark(dir.path(), "base");
        git(dir.path(), &["merge", "-q", "--ff-only", "upstream"]);
        fs::write(dir.path().join("a.txt"), "edited by the turn\n").unwrap();
        let after = mark(dir.path(), "1");

        // Act
        let diff = turn_diff(dir.path(), &before, &after, true).unwrap();

        // Assert
        assert_eq!(paths(&diff), ["a.txt"]);
        assert!(!diff.patch.contains("pulled.txt"), "{}", diff.patch);
        assert!(!diff.patch.contains("upstream edit"), "{}", diff.patch);
    }

    #[test]
    fn a_file_the_turn_edited_after_a_pull_changed_it_stays_in_the_change() {
        // Arrange
        let dir = repo();
        git(dir.path(), &["checkout", "-q", "-b", "upstream"]);
        fs::write(dir.path().join("a.txt"), "upstream\n").unwrap();
        git(dir.path(), &["commit", "-q", "-am", "upstream"]);
        git(dir.path(), &["checkout", "-q", "main"]);
        let before = mark(dir.path(), "base");
        git(dir.path(), &["merge", "-q", "--ff-only", "upstream"]);
        fs::write(dir.path().join("a.txt"), "upstream\nand the turn\n").unwrap();
        let after = mark(dir.path(), "1");

        // Act
        let diff = turn_diff(dir.path(), &before, &after, false).unwrap();

        // Assert
        assert_eq!(paths(&diff), ["a.txt"]);
    }

    #[test]
    fn a_commit_the_turn_made_stays_in_the_change() {
        // Arrange
        let dir = repo();
        let before = mark(dir.path(), "base");
        fs::write(dir.path().join("a.txt"), "edited and committed\n").unwrap();
        fs::write(dir.path().join("b.txt"), "also\n").unwrap();
        commit_now(dir.path(), "the turn commits");
        let after = mark(dir.path(), "1");

        // Act
        let diff = turn_diff(dir.path(), &before, &after, false).unwrap();

        // Assert
        assert_eq!(paths(&diff), ["a.txt", "b.txt"]);
    }

    #[test]
    fn a_session_in_a_subfolder_still_gets_the_files_of_the_whole_repository() {
        // Arrange
        let dir = repo();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/in.txt"), "in\n").unwrap();
        let sub = dir.path().join("sub");
        let before = mark(&sub, "base");
        fs::write(sub.join("in.txt"), "in\nmore\n").unwrap();
        fs::write(dir.path().join("a.txt"), "outside\n").unwrap();
        let after = mark(&sub, "1");

        // Act
        let diff = turn_diff(&sub, &before, &after, true).unwrap();

        // Assert
        assert_eq!(paths(&diff), ["a.txt", "sub/in.txt"]);
        assert!(
            diff.patch.contains("diff --git a/sub/in.txt b/sub/in.txt"),
            "{}",
            diff.patch
        );
    }

    #[test]
    fn a_rebase_that_moves_head_leaves_out_what_it_brought_in() {
        // Arrange
        let dir = repo();
        git(dir.path(), &["checkout", "-q", "-b", "topic"]);
        fs::write(dir.path().join("topic.txt"), "t\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "topic"]);
        git(dir.path(), &["checkout", "-q", "main"]);
        fs::write(dir.path().join("main.txt"), "m\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "main"]);
        git(dir.path(), &["checkout", "-q", "topic"]);
        let before = mark(dir.path(), "base");
        git(dir.path(), &["rebase", "-q", "main"]);
        fs::write(dir.path().join("a.txt"), "turn\n").unwrap();
        let after = mark(dir.path(), "1");

        // Act
        let diff = turn_diff(dir.path(), &before, &after, false).unwrap();

        // Assert
        assert_eq!(paths(&diff), ["a.txt"]);
    }

    #[test]
    fn taking_the_diff_leaves_the_users_index_and_head_alone() {
        // Arrange
        let dir = repo();
        let before = mark(dir.path(), "base");
        fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        fs::write(dir.path().join("c.txt"), "untracked\n").unwrap();
        let after = mark(dir.path(), "1");
        let index_before = fs::read(dir.path().join(".git/index")).unwrap();
        let head_before = git(dir.path(), &["rev-parse", "HEAD"]);

        // Act
        let _ = turn_diff(dir.path(), &before, &after, true).unwrap();

        // Assert
        assert_eq!(
            fs::read(dir.path().join(".git/index")).unwrap(),
            index_before
        );
        assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head_before);
        assert_eq!(
            git(dir.path(), &["status", "--porcelain"]),
            "M  a.txt\n?? c.txt\n"
        );
    }

    #[test]
    fn a_path_that_looks_like_a_pathspec_is_taken_literally() {
        // Arrange
        let dir = repo();
        let before = mark(dir.path(), "base");
        fs::write(dir.path().join("*.txt"), "star\n").unwrap();
        let after = mark(dir.path(), "1");

        // Act
        let diff = turn_diff(dir.path(), &before, &after, false).unwrap();

        // Assert
        assert_eq!(paths(&diff), ["*.txt"]);
    }

    #[test]
    fn a_diff_longer_than_the_cap_is_cut_at_a_line_end_and_marked() {
        // Arrange
        let dir = repo();
        let before = mark(dir.path(), "base");
        let line = "x".repeat(99) + "\n";
        fs::write(dir.path().join("big.txt"), line.repeat(8000)).unwrap();
        let after = mark(dir.path(), "1");

        // Act
        let diff = turn_diff(dir.path(), &before, &after, true).unwrap();

        // Assert
        assert!(diff.truncated);
        assert!(diff.patch.len() <= MAX_PATCH_BYTES);
        assert!(diff.patch.ends_with('\n'));
        assert_eq!(diff.files[0].added, 8000);
    }

    #[test]
    fn a_diff_of_too_many_lines_lists_the_files_and_gives_no_text() {
        // Arrange
        let dir = repo();
        let before = mark(dir.path(), "base");
        fs::write(dir.path().join("huge.txt"), "y\n".repeat(25_000)).unwrap();
        let after = mark(dir.path(), "1");

        // Act
        let diff = turn_diff(dir.path(), &before, &after, true).unwrap();

        // Assert
        assert_eq!(paths(&diff), ["huge.txt"]);
        assert_eq!(diff.patch, "");
        assert!(diff.truncated);
    }

    #[test]
    fn more_files_than_the_cap_are_listed_up_to_the_cap_and_marked() {
        // Arrange
        let dir = repo();
        let before = mark(dir.path(), "base");
        for n in 0..=MAX_FILES {
            fs::write(dir.path().join(format!("f{n:05}.txt")), "x\n").unwrap();
        }
        let after = mark(dir.path(), "1");

        // Act
        let diff = turn_diff(dir.path(), &before, &after, false).unwrap();

        // Assert
        assert_eq!(diff.files.len(), MAX_FILES);
        assert!(diff.truncated);
    }

    #[test]
    fn a_ref_that_does_not_exist_is_an_error() {
        // Arrange
        let dir = repo();
        let before = mark(dir.path(), "base");

        // Act
        let result = turn_diff(dir.path(), &before, "refs/coda/checkpoints/1/none", false);

        // Assert
        assert!(result.is_err());
    }
}
