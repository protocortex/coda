// SPDX-License-Identifier: AGPL-3.0-only

//! Builds the fake `claude` and `codex` binaries once per test process and returns where they are.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Attempts at the post-build smoke run, see [`ensure_runnable`].
const SMOKE_ATTEMPTS: u32 = 5;
/// How long one smoke run may take before it is killed and retried.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const CLAUDE_BIN: &str = "fake-claude";
const CODEX_BIN: &str = "fake-codex";
const MANIFEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
/// Parent of the private target dirs, one per binary, so a nested build never
/// rewrites a binary that a concurrent test process is executing from the shared one.
const TARGET_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target");

/// Why the fake binary could not be provided.
#[derive(Debug, Clone, thiserror::Error)]
pub enum BuildError {
    /// `cargo build` could not be started.
    #[error("could not run cargo to build a fake binary: {0}")]
    Spawn(String),
    /// `cargo build` ran and failed.
    #[error("cargo failed to build a fake binary: {0}")]
    Failed(String),
    /// The build succeeded but reported no executable.
    #[error("cargo built a fake binary but reported no executable path")]
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
    PATH.get_or_init(|| build(CLAUDE_BIN)).clone()
}

/// Builds `fake-codex` on first use and returns the path to the executable.
///
/// # Errors
/// Returns [`BuildError`] when cargo cannot build the binary.
pub fn fake_codex_path() -> Result<PathBuf, BuildError> {
    static PATH: OnceLock<Result<PathBuf, BuildError>> = OnceLock::new();
    PATH.get_or_init(|| build(CODEX_BIN)).clone()
}

fn build(bin_name: &str) -> Result<PathBuf, BuildError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "build",
            "--quiet",
            "--message-format=json",
            "--bin",
            bin_name,
        ])
        .args(["--manifest-path", MANIFEST, "--target-dir"])
        .arg(Path::new(TARGET_ROOT).join(bin_name))
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
        .filter(|message| message["target"]["name"] == bin_name)
        .find_map(|message| {
            // Cargo re-links the uplifted `executable` on every build, so a
            // concurrent test process could see it missing; the hashed copy
            // in `deps` is left alone when the build is fresh.
            let built = message["filenames"].get(0)?.as_str()?;
            Some(PathBuf::from(built))
        })
        .ok_or(BuildError::NoArtifact)?;
    ensure_runnable(bin_name, &path)?;
    Ok(path)
}

/// Runs the fresh binary until it starts cleanly.
///
/// macOS can kill a binary with SIGKILL when several test processes execute
/// it right after it was linked, so the first runs are retried.
fn ensure_runnable(bin_name: &str, path: &Path) -> Result<(), BuildError> {
    let mut last = String::new();
    for _ in 0..SMOKE_ATTEMPTS {
        match smoke_attempt(path, SMOKE_TIMEOUT) {
            Ok(()) => return Ok(()),
            Err(reason) => last = reason,
        }
    }
    Err(BuildError::Failed(format!(
        "{bin_name} does not start: {last}"
    )))
}

/// Runs `--version` with null stdio and kills the child once `timeout` passes,
/// so a binary that hangs on exec cannot block the test process.
fn smoke_attempt(path: &Path, timeout: Duration) -> Result<(), String> {
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| source.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(status.to_string()),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {timeout:?}"));
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(source) => return Err(source.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_hanging_binary_is_killed_and_reported_as_a_failed_attempt() {
        // Arrange
        let script = std::env::temp_dir().join(format!("hang-{}.sh", std::process::id()));
        std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();

        // Act
        let outcome = smoke_attempt(&script, Duration::from_millis(200));

        // Assert
        std::fs::remove_file(&script).unwrap();
        assert!(outcome.unwrap_err().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn the_returned_path_is_an_executable_that_reports_a_version() {
        // Arrange
        let path = fake_claude_path().unwrap();

        // Act
        let output = Command::new(&path)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .unwrap();

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
