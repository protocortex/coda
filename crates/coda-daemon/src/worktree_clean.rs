// SPDX-License-Identifier: Apache-2.0

//! Removing the worktrees coda made once they are no longer needed.
//!
//! Only a worktree that a session row records, at the path coda computes from
//! the session id, and that git lists as a worktree of the recorded repository,
//! is ever a candidate. A worktree with uncommitted changes, one whose session
//! is running or recently active, and anything the user made is kept. Folders
//! in the managed folder that no session owns are reported and never touched.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::{fs, io};

use coda_config::worktree::CleanupConfig;
use coda_protocol::methods::{CleanVerdict, CleanWorktree, UnknownFolder, WorktreeCleanResult};
use coda_protocol::types::SessionId;
use coda_selfhost::process::run_bounded;
use coda_store::WorktreeSession;
use tracing::warn;

use crate::worktree::{Worktrees, git, same_folder};

const MILLIS_PER_DAY: i64 = 86_400_000;
/// A session this recently active is left alone, so a worktree that was just
/// made is not removed before the first turn.
const RECENT_ACTIVITY: Duration = Duration::from_secs(3600);
const CLEAN_COMMAND_LIMIT: Duration = Duration::from_secs(600);
const PULL_REQUEST_LIMIT: Duration = Duration::from_secs(30);
const FALLBACK_DEFAULT_BRANCHES: [&str; 2] = ["main", "master"];
/// How many leftover paths an error names.
const IGNORED_SAMPLE: usize = 3;
const REMOTE_HEAD_REF: &str = "refs/remotes/origin/HEAD";

/// What cleanup needs to know about the world, so a test can stand in for it.
pub(crate) struct Probes<'a> {
    pub(crate) now_ms: i64,
    /// Whether a turn is running in the session.
    pub(crate) running: &'a dyn Fn(&SessionId) -> bool,
    /// Whether the branch's pull request is merged.
    pub(crate) merged: &'a dyn Fn(&Path, &str) -> bool,
}

/// Works out what to do with every worktree in `rows` and, when `apply` is set,
/// removes the ones that can go. A removal that fails is reported, not raised.
pub(crate) fn clean(
    trees: &Worktrees,
    rows: &[WorktreeSession],
    policy: &CleanupConfig,
    probes: &Probes<'_>,
    apply: bool,
) -> WorktreeCleanResult {
    let worktrees = rows
        .iter()
        .map(|row| {
            let mut found = inspect(trees, row, policy, probes);
            if apply && found.verdict == CleanVerdict::Remove {
                remove(trees, row, policy, probes, &mut found);
            }
            found
        })
        .collect();
    WorktreeCleanResult {
        applied: apply,
        worktrees,
        unknown: unknown_folders(trees, rows),
    }
}

/// Why a worktree is kept or removed, without changing anything.
fn inspect(
    trees: &Worktrees,
    row: &WorktreeSession,
    policy: &CleanupConfig,
    probes: &Probes<'_>,
) -> CleanWorktree {
    let path = PathBuf::from(&row.worktree.path);
    let report = |verdict, reason: &str, bytes| CleanWorktree {
        session_id: row.session.clone(),
        path: row.worktree.path.clone(),
        branch: row.worktree.branch.clone(),
        bytes,
        verdict,
        reason: reason.to_owned(),
        removed: false,
        error: None,
    };
    if !path.exists() {
        return report(CleanVerdict::Gone, "the folder is already gone", 0);
    }
    let bytes = folder_size(&path);
    if path != trees.path_for(&row.session)
        || row.worktree.branch != Worktrees::branch_for(&row.session)
    {
        return report(
            CleanVerdict::Keep,
            "the recorded folder is not the one coda keeps for this session",
            bytes,
        );
    }
    if (probes.running)(&row.session) {
        return report(CleanVerdict::Keep, "a turn is running", bytes);
    }
    let idle_ms = probes.now_ms.saturating_sub(row.updated_at);
    if idle_ms < i64::try_from(RECENT_ACTIVITY.as_millis()).unwrap_or(i64::MAX) {
        return report(
            CleanVerdict::Keep,
            "the session was active in the last hour",
            bytes,
        );
    }
    let repo = Path::new(&row.worktree.repo);
    if !is_registered(repo, &path) {
        return report(
            CleanVerdict::Keep,
            "git does not list it as a worktree of the recorded repository",
            bytes,
        );
    }
    match git(&path, &["status", "--porcelain"]) {
        Ok(status) if status.trim().is_empty() => {}
        Ok(_) => return report(CleanVerdict::Keep, "it has uncommitted changes", bytes),
        Err(_) => return report(CleanVerdict::Keep, "its state could not be read", bytes),
    }
    let branch = &row.worktree.branch;
    if (probes.merged)(repo, branch) {
        return report(CleanVerdict::Remove, "its pull request is merged", bytes);
    }
    if has_no_commits_past_default(repo, branch) {
        return report(
            CleanVerdict::Remove,
            "its branch has no commits past the default branch",
            bytes,
        );
    }
    let idle_days = u32::try_from(idle_ms / MILLIS_PER_DAY).unwrap_or(u32::MAX);
    if idle_days >= policy.idle_days {
        return report(
            CleanVerdict::Remove,
            &format!("the session has been idle for {idle_days} days"),
            bytes,
        );
    }
    report(CleanVerdict::Keep, "it is still in use", bytes)
}

/// Runs the clean command, removes the worktree, and deletes its branch only
/// when nothing is lost: its tip is the head of a merged pull request, or the
/// branch adds nothing. Files that `.gitignore` excludes are not committed
/// anywhere, so a worktree that still holds some after the clean command stays.
fn remove(
    trees: &Worktrees,
    row: &WorktreeSession,
    policy: &CleanupConfig,
    probes: &Probes<'_>,
    found: &mut CleanWorktree,
) {
    let path = trees.path_for(&row.session);
    let repo = Path::new(&row.worktree.repo);
    let branch = &row.worktree.branch;
    let nothing_lost = (probes.merged)(repo, branch) || has_no_commits_past_default(repo, branch);
    if let Some((program, args)) = policy.clean_command.split_first() {
        let done = run_bounded(
            Command::new(program).args(args).current_dir(&path),
            CLEAN_COMMAND_LIMIT,
        );
        let failure = match done {
            Ok(done) if done.status.success() => None,
            Ok(done) => Some(format!("the clean command failed ({})", done.status)),
            Err(e) => Some(format!("the clean command could not run: {e}")),
        };
        if let Some(failure) = failure {
            found.error = Some(failure);
            return;
        }
    }
    let _exclusive = trees.exclusive();
    if (probes.running)(&row.session) {
        found.error = Some("a turn started in the session".to_owned());
        return;
    }
    match git(&path, &["status", "--porcelain", "--ignored"]) {
        Ok(status) if status.trim().is_empty() => {}
        Ok(status) => {
            let sample: Vec<&str> = status.lines().take(IGNORED_SAMPLE).collect();
            found.error = Some(format!(
                "files remain that no commit holds, so nothing was removed: {}",
                sample.join(", ")
            ));
            return;
        }
        Err(e) => {
            found.error = Some(e.to_string());
            return;
        }
    }
    // No `--force`: git itself refuses a worktree that changed since the check.
    if let Err(e) = git(repo, &["worktree", "remove", &path.to_string_lossy()]) {
        warn!(error = %e, "could not remove a worktree");
        found.error = Some(e.to_string());
        return;
    }
    found.removed = true;
    if nothing_lost && let Err(e) = git(repo, &["branch", "-D", branch]) {
        warn!(error = %e, "could not delete a branch after removing its worktree");
    }
}

/// Whether git lists `path` as a worktree of `repo`.
fn is_registered(repo: &Path, path: &Path) -> bool {
    git(repo, &["worktree", "list", "--porcelain"]).is_ok_and(|listing| {
        listing
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .any(|listed| same_folder(Path::new(listed), path) || Path::new(listed) == path)
    })
}

fn default_branch(repo: &Path) -> Option<String> {
    if let Ok(head) = git(repo, &["symbolic-ref", "--short", REMOTE_HEAD_REF]) {
        return Some(head.trim().to_owned());
    }
    FALLBACK_DEFAULT_BRANCHES
        .iter()
        .find(|name| {
            git(
                repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{name}"),
                ],
            )
            .is_ok()
        })
        .map(|name| (*name).to_owned())
}

fn has_no_commits_past_default(repo: &Path, branch: &str) -> bool {
    let Some(default) = default_branch(repo) else {
        return false;
    };
    git(
        repo,
        &["rev-list", "--count", &format!("{default}..{branch}")],
    )
    .is_ok_and(|count| count.trim() == "0")
}

/// Asks `gh` whether a merged pull request ended at the commit the branch is
/// at now. A branch that moved on since the merge, or that only shares a name
/// with an old merged pull request, does not count. Any failure, such as no
/// `gh` or no login, reads as not merged.
pub(crate) fn pull_request_merged(repo: &Path, branch: &str) -> bool {
    let Ok(tip) = git(
        repo,
        &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
    ) else {
        return false;
    };
    let mut command = Command::new("gh");
    command
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "merged",
            "--json",
            "headRefOid",
        ])
        .current_dir(repo);
    let Ok(done) = run_bounded(&mut command, PULL_REQUEST_LIMIT) else {
        return false;
    };
    done.status.success() && merged_at(&done.stdout, tip.trim())
}

/// Whether `listing`, the JSON `gh pr list --json headRefOid` prints, holds a
/// pull request whose head is `tip`.
fn merged_at(listing: &str, tip: &str) -> bool {
    serde_json::from_str::<Vec<serde_json::Value>>(listing).is_ok_and(|found| {
        found
            .iter()
            .any(|pr| pr.get("headRefOid").and_then(serde_json::Value::as_str) == Some(tip))
    })
}

fn unknown_folders(trees: &Worktrees, rows: &[WorktreeSession]) -> Vec<UnknownFolder> {
    let owned: BTreeSet<String> = rows
        .iter()
        .map(|row| row.session.as_str().to_owned())
        .collect();
    let Ok(entries) = fs::read_dir(trees.root()) else {
        return Vec::new();
    };
    let mut found: Vec<UnknownFolder> = entries
        .filter_map(Result::ok)
        .filter(|entry| !owned.contains(&entry.file_name().to_string_lossy().into_owned()))
        .map(|entry| UnknownFolder {
            bytes: folder_size(&entry.path()),
            path: entry.path().to_string_lossy().into_owned(),
        })
        .collect();
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// Bytes under `path`, counting files and not following symbolic links.
fn folder_size(path: &Path) -> u64 {
    fn walk(path: &Path) -> io::Result<u64> {
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_dir() {
            return Ok(meta.len());
        }
        let mut total = 0;
        for entry in fs::read_dir(path)? {
            total += walk(&entry?.path()).unwrap_or(0);
        }
        Ok(total)
    }
    walk(path).unwrap_or(0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use coda_store::SessionWorktree;
    use tempfile::TempDir;

    const DAY_MS: i64 = MILLIS_PER_DAY;
    const NOW: i64 = 100 * MILLIS_PER_DAY;

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    struct World {
        repo: TempDir,
        trees: TempDir,
        worktrees: Worktrees,
    }

    fn world() -> World {
        let repo = TempDir::new().unwrap();
        run(repo.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("a.txt"), "one\n").unwrap();
        run(repo.path(), &["add", "."]);
        run(repo.path(), &["commit", "-q", "-m", "first"]);
        let trees = TempDir::new().unwrap();
        let worktrees = Worktrees::new(trees.path().join("trees"), Vec::new());
        World {
            repo,
            trees,
            worktrees,
        }
    }

    fn row(world: &World, id: &str, idle_days: i64) -> WorktreeSession {
        let session = SessionId::new(id);
        let made = world.worktrees.create(world.repo.path(), &session).unwrap();
        WorktreeSession {
            session,
            worktree: made,
            updated_at: NOW - idle_days * DAY_MS,
        }
    }

    fn policy() -> CleanupConfig {
        CleanupConfig {
            enabled: true,
            idle_days: 14,
            clean_command: Vec::new(),
        }
    }

    fn never(_: &SessionId) -> bool {
        false
    }

    fn unmerged(_: &Path, _: &str) -> bool {
        false
    }

    fn probes<'a>(
        running: &'a dyn Fn(&SessionId) -> bool,
        merged: &'a dyn Fn(&Path, &str) -> bool,
    ) -> Probes<'a> {
        Probes {
            now_ms: NOW,
            running,
            merged,
        }
    }

    fn commit_in(path: &str) {
        std::fs::write(Path::new(path).join("work.txt"), "w\n").unwrap();
        run(Path::new(path), &["add", "."]);
        run(Path::new(path), &["commit", "-q", "-m", "work"]);
    }

    fn only(result: &WorktreeCleanResult) -> &CleanWorktree {
        assert_eq!(result.worktrees.len(), 1);
        &result.worktrees[0]
    }

    #[test]
    fn a_report_changes_nothing() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 30)];

        // Act
        let result = clean(
            &w.worktrees,
            &rows,
            &policy(),
            &probes(&never, &unmerged),
            false,
        );

        // Assert
        assert_eq!(only(&result).verdict, CleanVerdict::Remove);
        assert!(!only(&result).removed);
        assert!(Path::new(&rows[0].worktree.path).exists());
        assert!(!result.applied);
    }

    #[test]
    fn a_merged_pull_request_removes_the_worktree_and_its_branch() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 2)];
        commit_in(&rows[0].worktree.path);
        let merged = |_: &Path, _: &str| true;

        // Act
        let result = clean(
            &w.worktrees,
            &rows,
            &policy(),
            &probes(&never, &merged),
            true,
        );

        // Assert
        assert!(only(&result).removed, "{:?}", only(&result));
        assert!(!Path::new(&rows[0].worktree.path).exists());
        let branches = run(w.repo.path(), &["branch", "--format=%(refname:short)"]);
        assert_eq!(branches.trim(), "main");
    }

    #[test]
    fn a_branch_with_no_new_commits_is_removed_with_its_branch() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 2)];

        // Act
        let result = clean(
            &w.worktrees,
            &rows,
            &policy(),
            &probes(&never, &unmerged),
            true,
        );

        // Assert
        assert!(only(&result).removed, "{:?}", only(&result));
        let branches = run(w.repo.path(), &["branch", "--format=%(refname:short)"]);
        assert_eq!(branches.trim(), "main");
    }

    #[test]
    fn an_idle_worktree_goes_but_its_branch_and_commits_stay() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 20)];
        commit_in(&rows[0].worktree.path);

        // Act
        let result = clean(
            &w.worktrees,
            &rows,
            &policy(),
            &probes(&never, &unmerged),
            true,
        );

        // Assert
        assert!(only(&result).removed, "{:?}", only(&result));
        assert!(!Path::new(&rows[0].worktree.path).exists());
        run(w.repo.path(), &["rev-parse", "--verify", "coda/1"]);
    }

    #[test]
    fn a_worktree_still_in_use_stays() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 2)];
        commit_in(&rows[0].worktree.path);

        // Act
        let result = clean(
            &w.worktrees,
            &rows,
            &policy(),
            &probes(&never, &unmerged),
            true,
        );

        // Assert
        assert_eq!(only(&result).verdict, CleanVerdict::Keep);
        assert!(Path::new(&rows[0].worktree.path).exists());
    }

    #[test]
    fn uncommitted_changes_keep_the_worktree_even_when_the_pull_request_is_merged() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 30)];
        std::fs::write(Path::new(&rows[0].worktree.path).join("wip.txt"), "x").unwrap();
        let merged = |_: &Path, _: &str| true;

        // Act
        let result = clean(
            &w.worktrees,
            &rows,
            &policy(),
            &probes(&never, &merged),
            true,
        );

        // Assert
        assert_eq!(only(&result).verdict, CleanVerdict::Keep);
        assert!(only(&result).reason.contains("uncommitted"));
        assert!(Path::new(&rows[0].worktree.path).join("wip.txt").exists());
    }

    #[test]
    fn a_running_or_recently_active_session_is_kept() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 30), row(&w, "2", 0)];
        let running = |id: &SessionId| id.as_str() == "1";

        // Act
        let result = clean(
            &w.worktrees,
            &rows,
            &policy(),
            &probes(&running, &unmerged),
            true,
        );

        // Assert
        assert!(
            result
                .worktrees
                .iter()
                .all(|w| w.verdict == CleanVerdict::Keep)
        );
        assert!(rows.iter().all(|r| Path::new(&r.worktree.path).exists()));
    }

    #[test]
    fn the_clean_command_runs_before_removal_and_a_failing_one_keeps_the_worktree() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 30)];
        let marker = w.trees.path().join("cleaned");
        let touch = CleanupConfig {
            clean_command: vec!["touch".to_owned(), marker.to_string_lossy().into_owned()],
            ..policy()
        };
        let failing = CleanupConfig {
            clean_command: vec!["false".to_owned()],
            ..policy()
        };

        // Act
        let failed = clean(
            &w.worktrees,
            &rows,
            &failing,
            &probes(&never, &unmerged),
            true,
        );
        let kept = Path::new(&rows[0].worktree.path).exists();
        let done = clean(
            &w.worktrees,
            &rows,
            &touch,
            &probes(&never, &unmerged),
            true,
        );

        // Assert
        assert!(!only(&failed).removed);
        assert!(only(&failed).error.is_some());
        assert!(kept);
        assert!(only(&done).removed);
        assert!(marker.exists());
    }

    #[test]
    fn a_worktree_the_user_made_is_never_removed_even_inside_the_managed_folder() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 30)];
        let theirs = w.worktrees.root().join("mine");
        run(
            w.repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "mine",
                theirs.to_str().unwrap(),
            ],
        );

        // Act
        let result = clean(
            &w.worktrees,
            &rows,
            &policy(),
            &probes(&never, &unmerged),
            true,
        );

        // Assert
        assert!(theirs.join("a.txt").exists());
        run(w.repo.path(), &["rev-parse", "--verify", "mine"]);
        assert_eq!(
            result
                .unknown
                .iter()
                .map(|u| u.path.clone())
                .collect::<Vec<_>>(),
            [theirs.to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn a_record_that_points_at_a_user_worktree_is_refused() {
        // Arrange
        let w = world();
        let theirs_root = TempDir::new().unwrap();
        let theirs = theirs_root.path().join("theirs");
        run(
            w.repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "mine",
                theirs.to_str().unwrap(),
            ],
        );
        let forged = WorktreeSession {
            session: SessionId::new("9"),
            worktree: SessionWorktree {
                path: theirs.to_string_lossy().into_owned(),
                branch: "mine".to_owned(),
                repo: w.repo.path().to_string_lossy().into_owned(),
            },
            updated_at: 0,
        };

        // Act
        let result = clean(
            &w.worktrees,
            std::slice::from_ref(&forged),
            &policy(),
            &probes(&never, &unmerged),
            true,
        );

        // Assert
        assert_eq!(only(&result).verdict, CleanVerdict::Keep);
        assert!(theirs.join("a.txt").exists());
        run(w.repo.path(), &["rev-parse", "--verify", "mine"]);
    }

    #[test]
    fn a_folder_that_is_already_gone_is_reported_as_gone() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 30)];
        std::fs::remove_dir_all(&rows[0].worktree.path).unwrap();

        // Act
        let result = clean(
            &w.worktrees,
            &rows,
            &policy(),
            &probes(&never, &unmerged),
            true,
        );

        // Assert
        assert_eq!(only(&result).verdict, CleanVerdict::Gone);
    }

    #[test]
    fn unknown_folders_are_listed_with_sizes_and_left_alone() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 30)];
        let stray = w.worktrees.root().join("stray");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("big.bin"), vec![0_u8; 4096]).unwrap();

        // Act
        let result = clean(
            &w.worktrees,
            &rows,
            &policy(),
            &probes(&never, &unmerged),
            true,
        );

        // Assert
        assert_eq!(result.unknown.len(), 1);
        assert_eq!(result.unknown[0].bytes, 4096);
        assert!(stray.join("big.bin").exists());
    }

    #[test]
    fn the_report_counts_the_bytes_of_each_worktree() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 30)];
        std::fs::write(
            Path::new(&rows[0].worktree.path).join("big.bin"),
            vec![0_u8; 8192],
        )
        .unwrap();

        // Act
        let result = clean(
            &w.worktrees,
            &rows,
            &policy(),
            &probes(&never, &unmerged),
            false,
        );

        // Assert
        assert!(only(&result).bytes >= 8192);
    }

    #[test]
    fn ignored_files_that_remain_keep_the_worktree() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 30)];
        let folder = Path::new(&rows[0].worktree.path);
        std::fs::write(folder.join(".gitignore"), "secret.env\n").unwrap();
        run(folder, &["add", ".gitignore"]);
        run(folder, &["commit", "-q", "-m", "ignore"]);
        std::fs::write(folder.join("secret.env"), "TOKEN=1\n").unwrap();

        // Act
        let result = clean(
            &w.worktrees,
            &rows,
            &policy(),
            &probes(&never, &unmerged),
            true,
        );

        // Assert
        assert!(!only(&result).removed);
        assert!(
            only(&result)
                .error
                .as_deref()
                .unwrap()
                .contains("secret.env")
        );
        assert!(folder.join("secret.env").exists());
    }

    #[test]
    fn a_session_that_starts_a_turn_before_the_removal_is_left_alone() {
        // Arrange
        let w = world();
        let rows = [row(&w, "1", 30)];
        let calls = std::cell::Cell::new(0);
        let late = |_: &SessionId| {
            calls.set(calls.get() + 1);
            calls.get() > 1
        };

        // Act
        let result = clean(
            &w.worktrees,
            &rows,
            &policy(),
            &probes(&late, &unmerged),
            true,
        );

        // Assert
        assert!(!only(&result).removed);
        assert!(Path::new(&rows[0].worktree.path).exists());
    }

    #[test]
    fn a_merged_pull_request_counts_only_when_it_ended_at_the_branch_tip() {
        // Arrange
        let listing = r#"[{"headRefOid":"aaa"},{"headRefOid":"bbb"}]"#;

        // Act and Assert
        assert!(merged_at(listing, "bbb"));
        assert!(!merged_at(listing, "ccc"));
        assert!(!merged_at("[]", "aaa"));
        assert!(!merged_at("not json", "aaa"));
    }
}
