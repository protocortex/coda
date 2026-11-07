// SPDX-License-Identifier: Apache-2.0

//! Git worktrees that coda makes for sessions.
//!
//! A worktree is a folder under the managed folder, named after its session,
//! on a branch `coda/<session>` that coda creates and nothing else uses. Coda
//! only ever adds a worktree at a path it computed from the session id and
//! never checks out, moves or deletes a branch or worktree it did not make: a
//! branch that already exists makes the creation fail and is left alone.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;
use std::{fs, io};

use coda_config::worktree::CleanupConfig;
use coda_protocol::types::SessionId;
use coda_selfhost::process::{ProcessError, run_bounded};
use coda_store::SessionWorktree;
use tracing::warn;

/// The prefix of every branch coda makes for a session.
pub const BRANCH_PREFIX: &str = "coda/";
/// The managed folder inside the data folder when the config names none.
pub const DEFAULT_DIR_NAME: &str = "worktrees";
const GIT_LIMIT: Duration = Duration::from_secs(60);
/// A setup command is usually an install, so it gets longer than git does.
const SETUP_LIMIT: Duration = Duration::from_secs(600);
const OWNER_ONLY_MODE: u32 = 0o700;
/// Variables that would point git at another repository than the one named.
const INHERITED_GIT_VARIABLES: [&str; 4] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
];
const GIT_DIR_NAME: &str = ".git";
const NOT_A_REPOSITORY_TEXT: &str = "not a git repository";

/// Why a worktree could not be made, found or put back. The text is for the caller.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("the session's folder is not inside a git repository")]
    NotARepository,
    #[error("the repository has no commit yet, so there is nothing to branch from")]
    NoCommit,
    #[error("{} already exists, so no worktree was made there", path.display())]
    PathTaken { path: PathBuf },
    #[error("git {args} failed ({status}): {stderr}")]
    Git {
        args: String,
        status: String,
        stderr: String,
    },
    #[error("the setup command failed ({status}): {stderr}")]
    Setup { status: String, stderr: String },
    #[error("the branch {branch} is checked out in {}, which is not this session's folder", path.display())]
    BranchInUse { branch: String, path: PathBuf },
    #[error("{} is not the folder coda keeps for this session, so it was not recreated", path.display())]
    NotManaged { path: PathBuf },
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
}

/// Makes and puts back the worktrees of sessions.
#[derive(Debug)]
pub struct Worktrees {
    root: PathBuf,
    setup: Vec<String>,
    cleanup: CleanupConfig,
    /// One git change at a time, so two requests cannot race on one path.
    busy: Mutex<()>,
}

impl Worktrees {
    /// Worktrees under `root`, which must be absolute, running `setup` (a
    /// program and its arguments, or nothing) in each new one.
    #[must_use]
    pub fn new(root: PathBuf, setup: Vec<String>) -> Self {
        Self {
            root,
            setup,
            cleanup: CleanupConfig::default(),
            busy: Mutex::new(()),
        }
    }

    /// Sets when `coda worktree clean` may remove worktrees.
    #[must_use]
    pub fn with_cleanup(mut self, cleanup: CleanupConfig) -> Self {
        self.cleanup = cleanup;
        self
    }

    /// When worktrees may be removed again.
    #[must_use]
    pub fn cleanup(&self) -> &CleanupConfig {
        &self.cleanup
    }

    /// Holds off every other worktree change until the guard is dropped.
    pub(crate) fn exclusive(&self) -> std::sync::MutexGuard<'_, ()> {
        self.busy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The folder every worktree lives in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where `session`'s worktree goes.
    #[must_use]
    pub fn path_for(&self, session: &SessionId) -> PathBuf {
        self.root.join(session.as_str())
    }

    /// The branch of `session`'s worktree.
    #[must_use]
    pub fn branch_for(session: &SessionId) -> String {
        format!("{BRANCH_PREFIX}{session}")
    }

    /// Makes `session`'s worktree from the `HEAD` of the repository holding
    /// `cwd`, on a new branch, and runs the setup command in it. When the setup
    /// fails the worktree and the branch it made are removed again.
    ///
    /// # Errors
    ///
    /// Returns a [`WorktreeError`] when `cwd` is not in a repository with a
    /// commit, when the path or the branch name is already taken, or when git
    /// or the setup command fails.
    pub fn create(
        &self,
        cwd: &Path,
        session: &SessionId,
    ) -> Result<SessionWorktree, WorktreeError> {
        let _one_at_a_time = self
            .busy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let repo = repository_root(cwd)?;
        git(
            &repo,
            &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        )
        .map_err(|_| WorktreeError::NoCommit)?;
        let path = self.path_for(session);
        let branch = Self::branch_for(session);
        self.prepare_root()?;
        if path.exists() {
            return Err(WorktreeError::PathTaken { path });
        }
        let path_str = path_text(&path);
        git(
            &repo,
            &["worktree", "add", "-q", "-b", &branch, &path_str, "HEAD"],
        )?;
        let made = SessionWorktree {
            path: path_str,
            branch,
            repo: path_text(&repo),
        };
        if let Err(failure) = self.run_setup(&path) {
            if let Err(cleanup) = remove_made(&made) {
                warn!(error = %cleanup, "could not remove a worktree whose setup failed");
            }
            return Err(failure);
        }
        Ok(made)
    }

    /// Puts back the folder of a worktree that was removed, from its branch
    /// when the branch is still there and from the repository's `HEAD`
    /// otherwise. Does nothing when the folder exists. Only the path that
    /// belongs to `session` is ever written; any other recorded path is refused.
    ///
    /// # Errors
    ///
    /// Returns [`WorktreeError::NotManaged`] when the recorded path is not the
    /// folder of `session` under the managed folder, or the errors of
    /// [`Worktrees::create`].
    pub fn restore(
        &self,
        session: &SessionId,
        recorded: &SessionWorktree,
    ) -> Result<(), WorktreeError> {
        let _one_at_a_time = self
            .busy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = self.path_for(session);
        if Path::new(&recorded.path) != path || recorded.branch != Self::branch_for(session) {
            return Err(WorktreeError::NotManaged {
                path: PathBuf::from(&recorded.path),
            });
        }
        if path.exists() {
            return Ok(());
        }
        let repo = repository_root(Path::new(&recorded.repo))?;
        self.prepare_root()?;
        let path_str = path_text(&path);
        let branch_ref = format!("refs/heads/{}", recorded.branch);
        // `--force` lets git reuse the registration the removed folder left behind.
        if git(&repo, &["show-ref", "--verify", "--quiet", &branch_ref]).is_ok() {
            if let Some(other) = checked_out_elsewhere(&repo, &branch_ref, &path)? {
                return Err(WorktreeError::BranchInUse {
                    branch: recorded.branch.clone(),
                    path: other,
                });
            }
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--force",
                    &path_str,
                    &recorded.branch,
                ],
            )?;
        } else {
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--force",
                    "-b",
                    &recorded.branch,
                    &path_str,
                    "HEAD",
                ],
            )?;
        }
        if let Err(failure) = self.run_setup(&path) {
            // The branch keeps the commits; only the unprepared folder goes, so
            // the next request tries again.
            if let Err(cleanup) = git(&repo, &["worktree", "remove", "--force", &path_str]) {
                warn!(error = %cleanup, "could not remove a restored folder whose setup failed");
            }
            return Err(failure);
        }
        Ok(())
    }

    fn prepare_root(&self) -> Result<(), WorktreeError> {
        let io = |source| WorktreeError::Io {
            path: self.root.clone(),
            source,
        };
        fs::create_dir_all(&self.root).map_err(io)?;
        set_owner_only(&self.root).map_err(io)
    }

    fn run_setup(&self, path: &Path) -> Result<(), WorktreeError> {
        let Some((program, args)) = self.setup.split_first() else {
            return Ok(());
        };
        let done = run_bounded(
            Command::new(program).args(args).current_dir(path),
            SETUP_LIMIT,
        )?;
        if done.status.success() {
            Ok(())
        } else {
            Err(WorktreeError::Setup {
                status: done.status.to_string(),
                stderr: done.stderr,
            })
        }
    }
}

/// Removes the worktree and the branch coda just made, and nothing else.
///
/// # Errors
///
/// Returns a [`WorktreeError`] when git fails.
pub fn remove_made(made: &SessionWorktree) -> Result<(), WorktreeError> {
    let repo = Path::new(&made.repo);
    git(repo, &["worktree", "remove", "--force", &made.path])?;
    git(repo, &["branch", "-D", &made.branch])?;
    Ok(())
}

/// The folder, other than `path`, where git has `branch_ref` checked out.
fn checked_out_elsewhere(
    repo: &Path,
    branch_ref: &str,
    path: &Path,
) -> Result<Option<PathBuf>, WorktreeError> {
    let listing = git(repo, &["worktree", "list", "--porcelain"])?;
    let mut folder = None;
    for line in listing.lines() {
        if let Some(found) = line.strip_prefix("worktree ") {
            folder = Some(PathBuf::from(found));
        } else if line.strip_prefix("branch ") == Some(branch_ref) {
            return Ok(folder.filter(|found| !same_folder(found, path)));
        }
    }
    Ok(None)
}

/// Whether two paths name one folder, even when it no longer exists, by
/// resolving the parents, which do.
pub(crate) fn same_folder(a: &Path, b: &Path) -> bool {
    let resolve = |path: &Path| {
        let parent = path.parent().and_then(|dir| fs::canonicalize(dir).ok())?;
        Some(parent.join(path.file_name()?))
    };
    resolve(a).is_some() && resolve(a) == resolve(b)
}

/// The root of the repository that owns `cwd`'s checkout, even when `cwd` is in
/// a linked worktree, so the record outlives that worktree.
fn repository_root(cwd: &Path) -> Result<PathBuf, WorktreeError> {
    let common = git(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map_err(|error| match error {
        WorktreeError::Git { stderr, .. } if stderr.contains(NOT_A_REPOSITORY_TEXT) => {
            WorktreeError::NotARepository
        }
        other => other,
    })?;
    let common = PathBuf::from(common.trim());
    if let Some(root) = common
        .parent()
        .filter(|_| common.file_name().is_some_and(|name| name == GIT_DIR_NAME))
    {
        return Ok(root.to_path_buf());
    }
    let top = git(
        cwd,
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
    )?;
    Ok(PathBuf::from(top.trim()))
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn set_owner_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(OWNER_ONLY_MODE))
}

pub(crate) fn git(cwd: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    for name in INHERITED_GIT_VARIABLES {
        command.env_remove(name);
    }
    let done = run_bounded(&mut command, GIT_LIMIT)?;
    if done.status.success() {
        Ok(done.stdout)
    } else {
        Err(WorktreeError::Git {
            args: args.join(" "),
            status: done.status.to_string(),
            stderr: done.stderr,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tempfile::TempDir;

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

    fn repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        run(dir.path(), &["init", "-q", "-b", "main"]);
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "first"]);
        dir
    }

    fn session(id: &str) -> SessionId {
        SessionId::new(id)
    }

    fn worktrees(root: &Path, setup: &[&str]) -> Worktrees {
        Worktrees::new(
            root.to_path_buf(),
            setup.iter().map(ToString::to_string).collect(),
        )
    }

    #[test]
    fn a_worktree_is_made_on_its_own_branch_from_head() {
        // Arrange
        let repo = repo();
        let trees = TempDir::new().unwrap();
        let wt = worktrees(&trees.path().join("trees"), &[]);

        // Act
        let made = wt.create(repo.path(), &session("7")).unwrap();

        // Assert
        let path = PathBuf::from(&made.path);
        assert_eq!(path, wt.path_for(&session("7")));
        assert_eq!(made.branch, "coda/7");
        assert_eq!(fs::read_to_string(path.join("a.txt")).unwrap(), "one\n");
        assert_eq!(
            run(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "coda/7"
        );
        assert_eq!(
            run(repo.path(), &["branch", "--show-current"]).trim(),
            "main"
        );
    }

    #[test]
    fn the_users_own_branches_and_worktrees_are_left_alone() {
        // Arrange
        let repo = repo();
        run(repo.path(), &["branch", "mine"]);
        let user_tree = TempDir::new().unwrap();
        let user_path = user_tree.path().join("mine");
        run(
            repo.path(),
            &["worktree", "add", "-q", user_path.to_str().unwrap(), "mine"],
        );
        let before = run(repo.path(), &["for-each-ref", "refs/heads"]);
        let trees = TempDir::new().unwrap();
        let wt = worktrees(trees.path(), &[]);

        // Act
        wt.create(repo.path(), &session("1")).unwrap();

        // Assert
        let after = run(repo.path(), &["for-each-ref", "refs/heads"]);
        assert!(before.lines().all(|line| after.lines().any(|l| l == line)));
        assert!(user_path.join("a.txt").exists());
        assert_eq!(
            run(&user_path, &["branch", "--show-current"]).trim(),
            "mine"
        );
    }

    #[test]
    fn a_branch_with_the_same_name_makes_it_fail_and_stays_untouched() {
        // Arrange
        let repo = repo();
        run(repo.path(), &["branch", "coda/3"]);
        let head = run(repo.path(), &["rev-parse", "coda/3"]);
        let trees = TempDir::new().unwrap();
        let wt = worktrees(trees.path(), &[]);

        // Act
        let result = wt.create(repo.path(), &session("3"));

        // Assert
        assert!(matches!(result, Err(WorktreeError::Git { .. })));
        assert_eq!(run(repo.path(), &["rev-parse", "coda/3"]), head);
        assert!(!wt.path_for(&session("3")).exists());
    }

    #[test]
    fn a_taken_path_is_refused_and_left_alone() {
        // Arrange
        let repo = repo();
        let trees = TempDir::new().unwrap();
        let wt = worktrees(trees.path(), &[]);
        let taken = wt.path_for(&session("4"));
        fs::create_dir_all(&taken).unwrap();
        fs::write(taken.join("keep.txt"), "x").unwrap();

        // Act
        let result = wt.create(repo.path(), &session("4"));

        // Assert
        assert!(matches!(result, Err(WorktreeError::PathTaken { .. })));
        assert!(taken.join("keep.txt").exists());
    }

    #[test]
    fn a_folder_without_a_repository_or_a_commit_is_refused() {
        // Arrange
        let trees = TempDir::new().unwrap();
        let wt = worktrees(trees.path(), &[]);
        let plain = TempDir::new().unwrap();
        let empty = TempDir::new().unwrap();
        run(empty.path(), &["init", "-q", "-b", "main"]);

        // Act
        let plain_result = wt.create(plain.path(), &session("1"));
        let empty_result = wt.create(empty.path(), &session("1"));

        // Assert
        assert!(matches!(plain_result, Err(WorktreeError::NotARepository)));
        assert!(matches!(empty_result, Err(WorktreeError::NoCommit)));
    }

    #[test]
    fn a_session_in_a_subfolder_gets_a_worktree_of_the_whole_repository() {
        // Arrange
        let repo = repo();
        fs::create_dir(repo.path().join("sub")).unwrap();
        let trees = TempDir::new().unwrap();
        let wt = worktrees(trees.path(), &[]);

        // Act
        let made = wt.create(&repo.path().join("sub"), &session("2")).unwrap();

        // Assert
        assert!(Path::new(&made.path).join("a.txt").exists());
        assert_eq!(
            fs::canonicalize(&made.repo).unwrap(),
            fs::canonicalize(repo.path()).unwrap()
        );
    }

    #[test]
    fn the_setup_command_runs_in_the_new_worktree() {
        // Arrange
        let repo = repo();
        let trees = TempDir::new().unwrap();
        let wt = worktrees(trees.path(), &["touch", "ready"]);

        // Act
        let made = wt.create(repo.path(), &session("5")).unwrap();

        // Assert
        assert!(Path::new(&made.path).join("ready").exists());
    }

    #[test]
    fn a_failing_setup_removes_what_was_made_and_nothing_else() {
        // Arrange
        let repo = repo();
        run(repo.path(), &["branch", "mine"]);
        let trees = TempDir::new().unwrap();
        let wt = worktrees(trees.path(), &["false"]);

        // Act
        let result = wt.create(repo.path(), &session("6"));

        // Assert
        assert!(matches!(result, Err(WorktreeError::Setup { .. })));
        assert!(!wt.path_for(&session("6")).exists());
        let branches = run(repo.path(), &["branch", "--format=%(refname:short)"]);
        assert_eq!(branches.lines().collect::<Vec<_>>(), ["main", "mine"]);
    }

    #[test]
    fn a_removed_checkout_comes_back_on_its_branch_with_its_commits() {
        // Arrange
        let repo = repo();
        let trees = TempDir::new().unwrap();
        let wt = worktrees(trees.path(), &[]);
        let made = wt.create(repo.path(), &session("8")).unwrap();
        let path = PathBuf::from(&made.path);
        fs::write(path.join("new.txt"), "work\n").unwrap();
        run(&path, &["add", "."]);
        run(&path, &["commit", "-q", "-m", "work"]);
        fs::remove_dir_all(&path).unwrap();

        // Act
        wt.restore(&session("8"), &made).unwrap();

        // Assert
        assert_eq!(fs::read_to_string(path.join("new.txt")).unwrap(), "work\n");
        assert_eq!(run(&path, &["branch", "--show-current"]).trim(), "coda/8");
    }

    #[test]
    fn a_removed_checkout_whose_branch_is_gone_comes_back_from_head() {
        // Arrange
        let repo = repo();
        let trees = TempDir::new().unwrap();
        let wt = worktrees(trees.path(), &[]);
        let made = wt.create(repo.path(), &session("9")).unwrap();
        run(repo.path(), &["worktree", "remove", "--force", &made.path]);
        run(repo.path(), &["branch", "-D", "coda/9"]);

        // Act
        wt.restore(&session("9"), &made).unwrap();

        // Assert
        let path = PathBuf::from(&made.path);
        assert_eq!(fs::read_to_string(path.join("a.txt")).unwrap(), "one\n");
    }

    #[test]
    fn a_path_outside_the_managed_folder_is_never_recreated() {
        // Arrange
        let repo = repo();
        let trees = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let wt = worktrees(trees.path(), &[]);
        let forged = SessionWorktree {
            path: elsewhere.path().join("9").to_string_lossy().into_owned(),
            branch: "coda/9".to_owned(),
            repo: repo.path().to_string_lossy().into_owned(),
        };

        // Act
        let result = wt.restore(&session("9"), &forged);

        // Assert
        assert!(matches!(result, Err(WorktreeError::NotManaged { .. })));
        assert!(!elsewhere.path().join("9").exists());
    }

    #[test]
    fn a_branch_checked_out_somewhere_else_is_not_taken_over() {
        // Arrange
        let repo = repo();
        let trees = TempDir::new().unwrap();
        let wt = worktrees(trees.path(), &[]);
        let made = wt.create(repo.path(), &session("5")).unwrap();
        run(repo.path(), &["worktree", "remove", "--force", &made.path]);
        let theirs = TempDir::new().unwrap();
        let theirs_path = theirs.path().join("theirs");
        run(
            repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                theirs_path.to_str().unwrap(),
                "coda/5",
            ],
        );

        // Act
        let result = wt.restore(&session("5"), &made);

        // Assert
        assert!(matches!(result, Err(WorktreeError::BranchInUse { .. })));
        assert!(!Path::new(&made.path).exists());
    }

    #[test]
    fn a_restore_whose_setup_fails_removes_the_folder_and_keeps_the_branch() {
        // Arrange
        let repo = repo();
        let trees = TempDir::new().unwrap();
        let made = worktrees(trees.path(), &[])
            .create(repo.path(), &session("3"))
            .unwrap();
        fs::remove_dir_all(&made.path).unwrap();
        let failing = worktrees(trees.path(), &["false"]);

        // Act
        let result = failing.restore(&session("3"), &made);

        // Assert
        assert!(matches!(result, Err(WorktreeError::Setup { .. })));
        assert!(!Path::new(&made.path).exists());
        run(repo.path(), &["rev-parse", "--verify", "coda/3"]);
    }

    #[test]
    fn a_session_started_in_a_linked_worktree_records_the_main_repository() {
        // Arrange
        let repo = repo();
        let linked_root = TempDir::new().unwrap();
        let linked = linked_root.path().join("linked");
        run(
            repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                linked.to_str().unwrap(),
            ],
        );
        let trees = TempDir::new().unwrap();
        let wt = worktrees(trees.path(), &[]);

        // Act
        let made = wt.create(&linked, &session("2")).unwrap();

        // Assert
        assert_eq!(
            fs::canonicalize(&made.repo).unwrap(),
            fs::canonicalize(repo.path()).unwrap()
        );
    }

    #[test]
    fn a_checkout_that_is_still_there_is_not_touched() {
        // Arrange
        let repo = repo();
        let trees = TempDir::new().unwrap();
        let wt = worktrees(trees.path(), &[]);
        let made = wt.create(repo.path(), &session("1")).unwrap();
        fs::write(Path::new(&made.path).join("dirty.txt"), "x").unwrap();

        // Act
        wt.restore(&session("1"), &made).unwrap();

        // Assert
        assert!(Path::new(&made.path).join("dirty.txt").exists());
    }
}
