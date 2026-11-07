// SPDX-License-Identifier: Apache-2.0

//! Per-turn snapshots of the working tree.
//!
//! A snapshot is a commit that nothing but a private ref points at, built from a
//! scratch copy of the index, so the user's index, `HEAD` and branches are never
//! touched. The refs live under `refs/coda/checkpoints/`, which `git log`,
//! `git branch` and `git status` do not show and `git gc` keeps.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{fs, process};

use coda_protocol::types::{SessionId, TurnId};
use coda_selfhost::process::{ProcessError, run_bounded};
use coda_store::{Store, StoreError};
use tracing::{debug, warn};

use crate::subscribe::Hub;

const BASELINE_LABEL: &str = "base";
/// The whole snapshot, not one git step: a turn holds its slot while it runs.
const SNAPSHOT_LIMIT: Duration = Duration::from_secs(30);
const RESTORE_LIMIT: Duration = Duration::from_secs(60);
const REF_PREFIX: &str = "refs/coda/checkpoints";
const SCRATCH_INDEX_PREFIX: &str = "coda-checkpoint-index";
const LOCK_SUFFIX: &str = ".lock";
const INDEX_LOCK_FILE: &str = "index.lock";
const COMMIT_MESSAGE: &str = "coda checkpoint";
const IDENTITY_NAME: &str = "coda";
const IDENTITY_EMAIL: &str = "coda@localhost";
const NOT_A_REPOSITORY_TEXT: &str = "not a git repository";
/// Variables that would point git at another repository or index than `cwd`'s.
const INHERITED_GIT_VARIABLES: [&str; 4] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
];
/// An empty old value makes `update-ref` refuse to replace a ref that exists.
const MUST_NOT_EXIST: &str = "";

static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
pub(crate) enum CheckpointError {
    #[error("not inside a git repository")]
    NotARepository,
    #[error("another git process holds the index lock")]
    IndexLocked,
    #[error("git {args} failed ({status}): {stderr}")]
    Git {
        args: String,
        status: String,
        stderr: String,
    },
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The ref of a session's baseline (`base`) or of one of its turns.
pub(crate) fn ref_name(session: &str, label: &str) -> String {
    format!("{REF_PREFIX}/{session}/{label}")
}

/// Snapshots the working tree of the repository holding `cwd` under `reference`:
/// tracked and untracked files, minus what `.gitignore` excludes. Nested
/// repositories that `git add` cannot take are left out. The commit's parent is
/// `HEAD`, or none when the repository has no commit yet. A ref that already
/// exists is never replaced, so two databases sharing a repository cannot
/// overwrite each other's snapshots.
pub(crate) fn snapshot(cwd: &Path, reference: &str) -> Result<(), CheckpointError> {
    let run = Git {
        cwd,
        deadline: Instant::now() + SNAPSHOT_LIMIT,
    };
    let common_dir = PathBuf::from(
        run.git(
            None,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .map_err(not_a_repository)?
        .trim(),
    );
    let user_index = PathBuf::from(
        run.git(
            None,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?
        .trim(),
    );
    let scratch = ScratchIndex::new(&common_dir, &user_index)?;
    let index = Some(scratch.path.as_path());
    let head = run
        .git(None, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .ok()
        .map(|out| out.trim().to_owned());
    match &head {
        Some(_) => {
            run.git(index, &["read-tree", "--reset", "HEAD"])?;
        }
        None => scratch.empty(),
    }
    if run.git(index, &["add", "-A"]).is_err() {
        run.add_without_nested_repositories(index)?;
    }
    let tree = run.git(index, &["write-tree"])?.trim().to_owned();
    let mut commit = vec!["commit-tree", &tree, "-m", COMMIT_MESSAGE];
    if let Some(parent) = &head {
        commit.extend(["-p", parent]);
    }
    let commit = run.git(index, &commit)?.trim().to_owned();
    run.git(None, &["update-ref", reference, &commit, MUST_NOT_EXIST])?;
    Ok(())
}

/// Puts the working tree of `cwd`'s folder back as `reference` holds it: files
/// the snapshot has come back, files made since are removed, and what
/// `.gitignore` excludes is left alone. The index ends up matching `HEAD`, as
/// it does after a snapshot, and `HEAD` and the branches do not move. Only the
/// folder `cwd` names is touched, not the rest of the repository.
pub(crate) fn restore(cwd: &Path, reference: &str) -> Result<(), CheckpointError> {
    let run = Git {
        cwd,
        deadline: Instant::now() + RESTORE_LIMIT,
    };
    let index = PathBuf::from(
        run.git(
            None,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )
        .map_err(not_a_repository)?
        .trim(),
    );
    let mut lock = index.into_os_string();
    lock.push(LOCK_SUFFIX);
    if Path::new(&lock).exists() {
        return Err(CheckpointError::IndexLocked);
    }
    let commit = run
        .git(
            None,
            &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
        )?
        .trim()
        .to_owned();
    run.git(
        None,
        &[
            "restore",
            "--source",
            &commit,
            "--worktree",
            "--staged",
            "--",
            ".",
        ],
    )
    .map_err(lock_failure)?;
    run.git(None, &["clean", "-fd", "--", "."])
        .map_err(lock_failure)?;
    run.git(None, &["reset", "-q", "--", "."])
        .map_err(lock_failure)?;
    Ok(())
}

/// A lock another git process took after the check at the start of a restore.
fn lock_failure(error: CheckpointError) -> CheckpointError {
    match error {
        CheckpointError::Git { stderr, .. } if stderr.contains(INDEX_LOCK_FILE) => {
            CheckpointError::IndexLocked
        }
        other => other,
    }
}

fn not_a_repository(error: CheckpointError) -> CheckpointError {
    match error {
        CheckpointError::Git { stderr, .. } if stderr.contains(NOT_A_REPOSITORY_TEXT) => {
            CheckpointError::NotARepository
        }
        other => other,
    }
}

/// Runs git in `cwd` until `deadline`.
struct Git<'a> {
    cwd: &'a Path,
    deadline: Instant,
}

impl Git<'_> {
    /// Runs `git args`, with `index` as the index when given, and returns its
    /// standard output.
    fn git(&self, index: Option<&Path>, args: &[&str]) -> Result<String, CheckpointError> {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(self.cwd)
            .env("GIT_AUTHOR_NAME", IDENTITY_NAME)
            .env("GIT_AUTHOR_EMAIL", IDENTITY_EMAIL)
            .env("GIT_COMMITTER_NAME", IDENTITY_NAME)
            .env("GIT_COMMITTER_EMAIL", IDENTITY_EMAIL)
            .env("GIT_OPTIONAL_LOCKS", "0");
        for name in INHERITED_GIT_VARIABLES {
            command.env_remove(name);
        }
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        let left = self.deadline.saturating_duration_since(Instant::now());
        let done = run_bounded(&mut command, left)?;
        if done.status.success() {
            Ok(done.stdout)
        } else {
            Err(CheckpointError::Git {
                args: args.join(" "),
                status: done.status.to_string(),
                stderr: done.stderr,
            })
        }
    }

    /// Retries `git add -A` leaving out every untracked directory that is itself
    /// a repository, which is what makes the first attempt fail.
    fn add_without_nested_repositories(&self, index: Option<&Path>) -> Result<(), CheckpointError> {
        let listing = self.git(
            index,
            &[
                "ls-files",
                "-z",
                "--others",
                "--exclude-standard",
                "--full-name",
            ],
        )?;
        let mut args = vec![
            "add".to_owned(),
            "-A".to_owned(),
            "--".to_owned(),
            ":/".to_owned(),
        ];
        for nested in listing.split('\0').filter(|entry| entry.ends_with('/')) {
            args.push(format!(":(top,exclude,literal){nested}"));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.git(index, &args)?;
        Ok(())
    }
}

/// Copies `from` to `to` and gives the copy the modification time of `from`.
/// Git calls an entry racily clean when it is as new as its index file, and
/// then reads the file instead of trusting the stat data. A copy with a newer
/// time would hide that, and a file edited in the same second, at the same
/// size, would be snapshotted with its old content.
///
/// The copy is created with default permissions, not the source's, so a read-only
/// index still gives a copy git can write. A copy that cannot be finished is removed.
fn copy_keeping_time(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut source = fs::File::open(from)?;
    let modified = source.metadata()?.modified()?;
    let mut copy = fs::File::create(to)?;
    let done = std::io::copy(&mut source, &mut copy).and_then(|_| copy.set_modified(modified));
    if done.is_err() {
        let _ = fs::remove_file(to);
    }
    done
}

/// A copy of the user's index that is removed when dropped, with the lock file
/// a killed `git add` leaves beside it. The copy keeps git's stat cache, so
/// `add -A` rehashes only the files that changed.
struct ScratchIndex {
    path: PathBuf,
}

impl ScratchIndex {
    fn new(common_dir: &Path, user_index: &Path) -> Result<Self, CheckpointError> {
        let name = format!(
            "{SCRATCH_INDEX_PREFIX}-{}-{}",
            process::id(),
            SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = common_dir.join(name);
        if user_index.exists() {
            copy_keeping_time(user_index, &path).map_err(|source| CheckpointError::Io {
                path: path.clone(),
                source,
            })?;
        }
        Ok(Self { path })
    }

    fn empty(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let mut lock = self.path.clone().into_os_string();
        lock.push(LOCK_SUFFIX);
        let _ = fs::remove_file(lock);
    }
}

/// Takes the baseline snapshot of a session before its first turn changes
/// anything. A session that already has a snapshot, from an earlier turn or from
/// before the baseline could be taken, keeps the one it has.
pub(crate) async fn before_turn(hub: &Arc<Hub>, session: &SessionId, turn: &TurnId) {
    let id = session.clone();
    let known = hub
        .with_store(move |store| {
            let record = store.load_session(&id)?;
            let kept = store
                .turn_checkpoints(&id)?
                .iter()
                .any(|c| c.before.is_some() || c.after.is_some());
            Ok((record.cwd, kept))
        })
        .await;
    let Ok((cwd, false)) = known else { return };
    let reference = ref_name(session.as_str(), BASELINE_LABEL);
    if take(&cwd, &reference).await {
        let (id, turn_id) = (session.clone(), turn.clone());
        record(hub, session, move |store| {
            store.record_checkpoint_before(&id, &turn_id, &reference)
        })
        .await;
    }
}

/// Takes the snapshot of the working tree as `turn` left it.
pub(crate) async fn after_turn(hub: &Arc<Hub>, session: &SessionId, turn: &TurnId) {
    let id = session.clone();
    let Ok(stored) = hub.with_store(move |store| store.load_session(&id)).await else {
        return;
    };
    let reference = ref_name(session.as_str(), turn.as_str());
    if take(&stored.cwd, &reference).await {
        let (id, turn_id) = (session.clone(), turn.clone());
        record(hub, session, move |store| {
            store.record_checkpoint_after(&id, &turn_id, &reference)
        })
        .await;
    }
}

/// Snapshots on the blocking pool. A failure is logged and never stops a turn;
/// a folder that is not a repository is the ordinary case and stays quiet.
async fn take(cwd: &str, reference: &str) -> bool {
    let (cwd, reference) = (PathBuf::from(cwd), reference.to_owned());
    let named = reference.clone();
    let result = tokio::task::spawn_blocking(move || snapshot(&cwd, &reference)).await;
    match result {
        Ok(Ok(())) => true,
        Ok(Err(CheckpointError::NotARepository)) => {
            debug!(reference = %named, "the folder is not a git repository; no checkpoint");
            false
        }
        Ok(Err(error)) => {
            warn!(reference = %named, %error, "could not take a checkpoint");
            false
        }
        Err(error) => {
            warn!(reference = %named, %error, "the checkpoint task failed");
            false
        }
    }
}

async fn record(
    hub: &Arc<Hub>,
    session: &SessionId,
    write: impl FnOnce(&mut Store) -> Result<(), StoreError> + Send + 'static,
) {
    if let Err(error) = hub.with_store_mut(write).await {
        warn!(session = %session, %error, "could not record a checkpoint");
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    const REF: &str = "refs/coda/checkpoints/1/1";
    const OLD_INDEX_AGE: Duration = Duration::from_secs(10);

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
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "first"]);
        dir
    }

    fn file_at(dir: &Path, reference: &str, name: &str) -> Option<String> {
        let out = Command::new("git")
            .args(["show", &format!("{reference}:{name}")])
            .current_dir(dir)
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap())
    }

    fn set_mtime(path: &Path, time: std::time::SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn a_same_size_edit_made_in_the_tick_git_wrote_the_index_is_still_snapshotted() {
        // Arrange
        let dir = repo();
        git(dir.path(), &["config", "core.trustctime", "false"]);
        let file = dir.path().join("a.txt");
        // Git compares times in whole seconds, so the tick must be a second or more ago.
        let tick = std::time::SystemTime::now() - OLD_INDEX_AGE;
        set_mtime(&file, tick);
        git(dir.path(), &["update-index", "--really-refresh"]);
        fs::write(&file, "two\n").unwrap();
        set_mtime(&file, tick);
        set_mtime(&dir.path().join(".git/index"), tick);
        assert_eq!(
            git(dir.path(), &["diff-files", "--name-only"]),
            "a.txt\n",
            "git itself sees the edit, so the setup is the racy case"
        );

        // Act
        snapshot(dir.path(), REF).unwrap();

        // Assert
        assert_eq!(file_at(dir.path(), REF, "a.txt").as_deref(), Some("two\n"));
    }

    #[test]
    fn a_read_only_index_is_snapshotted_and_leaves_no_scratch_copy() {
        // Arrange
        let dir = repo();
        fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let index = dir.path().join(".git/index");
        fs::set_permissions(&index, std::os::unix::fs::PermissionsExt::from_mode(0o444)).unwrap();

        // Act
        let result = snapshot(dir.path(), REF);

        // Assert
        result.unwrap();
        assert_eq!(file_at(dir.path(), REF, "a.txt").as_deref(), Some("two\n"));
        let leftovers = fs::read_dir(dir.path().join(".git"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("checkpoint")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn a_snapshot_holds_tracked_edits_and_untracked_files() {
        // Arrange
        let dir = repo();
        fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        fs::write(dir.path().join("new.txt"), "fresh\n").unwrap();

        // Act
        snapshot(dir.path(), REF).unwrap();

        // Assert
        assert_eq!(file_at(dir.path(), REF, "a.txt").as_deref(), Some("two\n"));
        assert_eq!(
            file_at(dir.path(), REF, "new.txt").as_deref(),
            Some("fresh\n")
        );
    }

    #[test]
    fn a_snapshot_leaves_head_branch_index_and_status_alone() {
        // Arrange
        let dir = repo();
        fs::write(dir.path().join("a.txt"), "staged\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        fs::write(dir.path().join("a.txt"), "staged then edited\n").unwrap();
        fs::write(dir.path().join("loose.txt"), "x\n").unwrap();
        let head = git(dir.path(), &["rev-parse", "HEAD"]);
        let branches = git(dir.path(), &["branch", "--list"]);
        let status = git(dir.path(), &["status", "--porcelain=v2"]);
        let index = fs::read(dir.path().join(".git/index")).unwrap();

        // Act
        snapshot(dir.path(), REF).unwrap();

        // Assert
        assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head);
        assert_eq!(git(dir.path(), &["branch", "--list"]), branches);
        assert_eq!(git(dir.path(), &["status", "--porcelain=v2"]), status);
        assert_eq!(fs::read(dir.path().join(".git/index")).unwrap(), index);
        assert_eq!(git(dir.path(), &["log", "--oneline"]).lines().count(), 1);
    }

    #[test]
    fn the_snapshot_is_a_child_of_head_and_no_scratch_file_is_left() {
        // Arrange
        let dir = repo();
        let head = git(dir.path(), &["rev-parse", "HEAD"]);

        // Act
        snapshot(dir.path(), REF).unwrap();

        // Assert
        let parent = git(dir.path(), &["rev-parse", &format!("{REF}^")]);
        assert_eq!(parent, head);
        let leftovers: Vec<_> = fs::read_dir(dir.path().join(".git"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("checkpoint"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn ignored_files_are_not_snapshotted() {
        // Arrange
        let dir = repo();
        fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        git(dir.path(), &["add", ".gitignore"]);
        git(dir.path(), &["commit", "-q", "-m", "ignore"]);
        fs::create_dir(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target/big.bin"), "x").unwrap();

        // Act
        snapshot(dir.path(), REF).unwrap();

        // Assert
        assert_eq!(file_at(dir.path(), REF, "target/big.bin"), None);
    }

    #[test]
    fn a_repository_with_no_commit_is_snapshotted_without_a_parent() {
        // Arrange
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();

        // Act
        snapshot(dir.path(), REF).unwrap();

        // Assert
        assert_eq!(file_at(dir.path(), REF, "a.txt").as_deref(), Some("one\n"));
        let parents = git(dir.path(), &["rev-list", "--parents", "-n1", REF]);
        assert_eq!(parents.split_whitespace().count(), 1);
    }

    #[test]
    fn a_nested_repository_without_a_commit_is_left_out_and_the_rest_is_kept() {
        // Arrange
        let dir = repo();
        let nested = dir.path().join("vendor/lib");
        fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "-q", "-b", "main"]);
        fs::write(nested.join("x.txt"), "x\n").unwrap();
        fs::write(dir.path().join("kept.txt"), "kept\n").unwrap();

        // Act
        snapshot(dir.path(), REF).unwrap();

        // Assert
        assert_eq!(
            file_at(dir.path(), REF, "kept.txt").as_deref(),
            Some("kept\n")
        );
        assert_eq!(file_at(dir.path(), REF, "vendor/lib/x.txt"), None);
    }

    #[test]
    fn a_folder_that_is_not_a_repository_is_reported_as_such() {
        // Arrange
        let dir = TempDir::new().unwrap();

        // Act
        let result = snapshot(dir.path(), REF);

        // Assert
        assert!(matches!(result, Err(CheckpointError::NotARepository)));
    }

    #[test]
    fn a_subfolder_snapshots_the_whole_repository() {
        // Arrange
        let dir = repo();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("top.txt"), "top\n").unwrap();

        // Act
        snapshot(&dir.path().join("sub"), REF).unwrap();

        // Assert
        assert_eq!(
            file_at(dir.path(), REF, "top.txt").as_deref(),
            Some("top\n")
        );
    }

    #[test]
    fn a_linked_worktree_leaves_the_main_index_alone() {
        // Arrange
        let main = repo();
        let linked = TempDir::new().unwrap();
        let path = linked.path().join("wt");
        git(
            main.path(),
            &["worktree", "add", "-q", path.to_str().unwrap()],
        );
        fs::write(path.join("w.txt"), "w\n").unwrap();
        let index = fs::read(main.path().join(".git/index")).unwrap();

        // Act
        snapshot(&path, REF).unwrap();

        // Assert
        assert_eq!(file_at(main.path(), REF, "w.txt").as_deref(), Some("w\n"));
        assert_eq!(fs::read(main.path().join(".git/index")).unwrap(), index);
    }

    #[test]
    fn a_nested_repository_whose_name_has_glob_characters_is_still_left_out() {
        // Arrange
        let dir = repo();
        let nested = dir.path().join("vendor/lib[1]");
        fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "-q", "-b", "main"]);
        fs::write(nested.join("x.txt"), "x\n").unwrap();
        fs::write(dir.path().join("kept.txt"), "kept\n").unwrap();

        // Act
        snapshot(dir.path(), REF).unwrap();

        // Assert
        assert_eq!(
            file_at(dir.path(), REF, "kept.txt").as_deref(),
            Some("kept\n")
        );
    }

    #[test]
    fn an_existing_ref_is_never_replaced() {
        // Arrange
        let dir = repo();
        snapshot(dir.path(), REF).unwrap();
        let first = git(dir.path(), &["rev-parse", REF]);
        fs::write(dir.path().join("a.txt"), "changed\n").unwrap();

        // Act
        let second = snapshot(dir.path(), REF);

        // Assert
        assert!(matches!(second, Err(CheckpointError::Git { .. })));
        assert_eq!(git(dir.path(), &["rev-parse", REF]), first);
    }

    fn read(dir: &Path, name: &str) -> Option<String> {
        fs::read_to_string(dir.join(name)).ok()
    }

    #[test]
    fn a_restore_brings_back_edits_deletions_and_untracked_files_and_drops_later_ones() {
        // Arrange
        let dir = repo();
        fs::write(dir.path().join("kept.txt"), "kept\n").unwrap();
        snapshot(dir.path(), REF).unwrap();
        fs::write(dir.path().join("a.txt"), "edited\n").unwrap();
        fs::remove_file(dir.path().join("kept.txt")).unwrap();
        fs::write(dir.path().join("later.txt"), "later\n").unwrap();

        // Act
        restore(dir.path(), REF).unwrap();

        // Assert
        assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("one\n"));
        assert_eq!(read(dir.path(), "kept.txt").as_deref(), Some("kept\n"));
        assert_eq!(read(dir.path(), "later.txt"), None);
    }

    #[test]
    fn a_restore_leaves_head_the_index_and_ignored_files_alone() {
        // Arrange
        let dir = repo();
        fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        git(dir.path(), &["add", ".gitignore"]);
        git(dir.path(), &["commit", "-q", "-m", "ignore"]);
        snapshot(dir.path(), REF).unwrap();
        fs::create_dir(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target/big.bin"), "x").unwrap();
        fs::write(dir.path().join("a.txt"), "edited\n").unwrap();
        let head = git(dir.path(), &["rev-parse", "HEAD"]);

        // Act
        restore(dir.path(), REF).unwrap();

        // Assert
        assert_eq!(read(dir.path(), "target/big.bin").as_deref(), Some("x"));
        assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head);
        assert_eq!(git(dir.path(), &["diff", "--cached", "--name-only"]), "");
        assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_restore_in_a_subfolder_leaves_the_rest_of_the_repository_alone() {
        // Arrange
        let dir = repo();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/s.txt"), "s\n").unwrap();
        snapshot(&dir.path().join("sub"), REF).unwrap();
        fs::write(dir.path().join("sub/s.txt"), "edited\n").unwrap();
        fs::write(dir.path().join("a.txt"), "edited too\n").unwrap();

        // Act
        restore(&dir.path().join("sub"), REF).unwrap();

        // Assert
        assert_eq!(read(dir.path(), "sub/s.txt").as_deref(), Some("s\n"));
        assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("edited too\n"));
    }

    #[test]
    fn a_held_index_lock_stops_a_restore_before_it_changes_anything() {
        // Arrange
        let dir = repo();
        snapshot(dir.path(), REF).unwrap();
        fs::write(dir.path().join("a.txt"), "edited\n").unwrap();
        fs::write(dir.path().join(".git/index.lock"), "").unwrap();

        // Act
        let result = restore(dir.path(), REF);

        // Assert
        assert!(matches!(result, Err(CheckpointError::IndexLocked)));
        assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("edited\n"));
    }

    #[test]
    fn a_restore_in_a_repository_with_no_commit_brings_back_the_snapshot() {
        // Arrange
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        snapshot(dir.path(), REF).unwrap();
        fs::write(dir.path().join("a.txt"), "edited\n").unwrap();
        fs::write(dir.path().join("later.txt"), "later\n").unwrap();

        // Act
        restore(dir.path(), REF).unwrap();

        // Assert
        assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("one\n"));
        assert_eq!(read(dir.path(), "later.txt"), None);
    }

    #[test]
    fn a_lock_failure_from_git_is_reported_as_a_held_index_lock() {
        // Arrange
        let failed = CheckpointError::Git {
            args: "restore".to_owned(),
            status: "exit status: 128".to_owned(),
            stderr: "fatal: Unable to create '/r/.git/index.lock': File exists.".to_owned(),
        };

        // Act
        let mapped = lock_failure(failed);

        // Assert
        assert!(matches!(mapped, CheckpointError::IndexLocked));
    }

    #[test]
    fn a_restore_of_a_missing_ref_fails_and_changes_nothing() {
        // Arrange
        let dir = repo();
        fs::write(dir.path().join("a.txt"), "edited\n").unwrap();

        // Act
        let result = restore(dir.path(), REF);

        // Assert
        assert!(matches!(result, Err(CheckpointError::Git { .. })));
        assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("edited\n"));
    }

    #[test]
    fn ref_names_sit_under_the_private_prefix() {
        assert_eq!(ref_name("7", "3"), "refs/coda/checkpoints/7/3");
        assert_eq!(ref_name("7", "base"), "refs/coda/checkpoints/7/base");
    }
}
