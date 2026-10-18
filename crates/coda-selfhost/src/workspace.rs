// SPDX-License-Identifier: AGPL-3.0-only

//! The temporary copy of the repository an attempt works in.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::error::SelfhostError;
use crate::process::run_bounded;

const GIT_LIMIT: Duration = Duration::from_secs(60);
/// Commit identity for the baseline, so no global git config is needed.
const IDENTITY: [&str; 6] = [
    "-c",
    "user.name=selfhost",
    "-c",
    "user.email=selfhost@localhost",
    "-c",
    "commit.gpgsign=false",
];

/// Copies the tracked regular files of the git repository at `source` into
/// `dest` and commits them there as the baseline the attempt is diffed against.
/// Untracked files and symlinks are never copied.
pub fn prepare_copy(source: &Path, dest: &Path) -> Result<(), SelfhostError> {
    let listing = git(source, &["ls-files", "-z", "--cached"])?;
    for relative in listing.split('\0').filter(|name| !name.is_empty()) {
        let from = source.join(relative);
        // Deleted files have nothing to copy; a symlink could lead outside the tree.
        if !fs::symlink_metadata(&from).is_ok_and(|meta| meta.is_file()) {
            continue;
        }
        let to = dest.join(relative);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
        }
        fs::copy(&from, &to).map_err(|e| io_error(&from, e))?;
    }
    git(dest, &["init", "-q"])?;
    git(dest, &["add", "-A"])?;
    let mut commit = IDENTITY.to_vec();
    commit.extend(["commit", "-q", "-m", "baseline"]);
    git(dest, &commit)?;
    Ok(())
}

/// Runs `git args` in `dir` and returns its standard output.
pub fn git(dir: &Path, args: &[&str]) -> Result<String, SelfhostError> {
    let done = run_bounded(Command::new("git").args(args).current_dir(dir), GIT_LIMIT)?;
    if !done.status.success() {
        return Err(SelfhostError::CommandFailed {
            what: format!("git {}", args.join(" ")),
            status: done.status.to_string(),
            stderr: done.stderr,
        });
    }
    Ok(done.stdout)
}

fn io_error(path: &Path, source: std::io::Error) -> SelfhostError {
    SelfhostError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tracked_regular_files_are_copied() {
        // Arrange
        let root = tempfile::tempdir().unwrap();
        let (source, dest) = (root.path().join("s"), root.path().join("d"));
        let outside = root.path().join("o");
        fs::create_dir_all(&source).unwrap();
        fs::write(&outside, "outside").unwrap();
        fs::write(source.join("a.rs"), "a").unwrap();
        fs::write(source.join(".env.local"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, source.join("link")).unwrap();
        git(&source, &["init", "-q"]).unwrap();
        git(&source, &["add", "a.rs", "link"]).unwrap();

        // Act
        prepare_copy(&source, &dest).unwrap();

        // Assert
        assert!(dest.join("a.rs").is_file());
        assert!(!dest.join(".env.local").exists());
        assert!(!dest.join("link").exists());
    }
}
