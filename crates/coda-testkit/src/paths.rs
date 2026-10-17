// SPDX-License-Identifier: AGPL-3.0-only

//! Builds the `fake-claude` binary once per test process and returns where it is.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Attempts at the post-build smoke run, see [`ensure_runnable`].
const SMOKE_ATTEMPTS: u32 = 5;
const BIN_NAME: &str = "fake-claude";
const MANIFEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
/// A private target dir, so a nested build never rewrites a binary that a
/// concurrent test process is executing from the shared one.
const TARGET_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/fake-claude");

/// Why the fake binary could not be provided.
#[derive(Debug, Clone, thiserror::Error)]
pub enum BuildError {
    /// `cargo build` could not be started.
    #[error("could not run cargo to build {BIN_NAME}: {0}")]
    Spawn(String),
    /// `cargo build` ran and failed.
    #[error("cargo failed to build {BIN_NAME}: {0}")]
    Failed(String),
    /// The build succeeded but reported no executable.
    #[error("cargo built {BIN_NAME} but reported no executable path")]
    NoArtifact,
}

/// Builds `fake-claude` on first use and returns the path to the executable.
///
/// Tests pass this path to the backend through config, so the backend runs
/// the real spawn path against a scripted binary.
///
/// # Errors
/// Returns [`BuildError`] when cargo cannot build the binary.
pub fn fake_claude_path() -> Result<PathBuf, BuildError> {
    static PATH: OnceLock<Result<PathBuf, BuildError>> = OnceLock::new();
    PATH.get_or_init(build).clone()
}

fn build() -> Result<PathBuf, BuildError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "build",
            "--quiet",
            "--message-format=json",
            "--bin",
            BIN_NAME,
        ])
        .args(["--manifest-path", MANIFEST, "--target-dir", TARGET_DIR])
        .output()
        .map_err(|source| BuildError::Spawn(source.to_string()))?;
    if !output.status.success() {
        return Err(BuildError::Failed(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .filter(|message| message["target"]["name"] == BIN_NAME)
        .find_map(|message| {
            // Cargo re-links the uplifted `executable` on every build, so a
            // concurrent test process could see it missing; the hashed copy
            // in `deps` is left alone when the build is fresh.
            let built = message["filenames"].get(0)?.as_str()?;
            Some(PathBuf::from(built))
        })
        .ok_or(BuildError::NoArtifact)?;
    ensure_runnable(&path)?;
    Ok(path)
}

/// Runs the fresh binary until it starts cleanly.
///
/// macOS can kill a binary with SIGKILL when several test processes execute
/// it right after it was linked, so the first runs are retried.
fn ensure_runnable(path: &Path) -> Result<(), BuildError> {
    let mut last = String::new();
    for _ in 0..SMOKE_ATTEMPTS {
        match Command::new(path).arg("--version").status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => last = status.to_string(),
            Err(source) => last = source.to_string(),
        }
    }
    Err(BuildError::Failed(format!(
        "{BIN_NAME} does not start: {last}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn the_returned_path_is_an_executable_that_reports_a_version() {
        // Arrange
        let path = fake_claude_path().unwrap();

        // Act
        let output = Command::new(&path).arg("--version").output().unwrap();

        // Assert
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "2.1.292 (Claude Code)"
        );
    }

    #[test]
    fn repeated_calls_return_the_same_path() {
        // Act
        let first = fake_claude_path().unwrap();
        let second = fake_claude_path().unwrap();

        // Assert
        assert_eq!(first, second);
    }
}
