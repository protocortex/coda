// SPDX-License-Identifier: Apache-2.0

//! Compacts the conversation of a Claude Code session with the installed `claude`.
//!
//! A turn of a coda session runs `claude` with `--disable-slash-commands` for the
//! `project` and `lean` profiles, and with that flag the text `/compact` does
//! nothing (checked with claude 2.1.296: no model call, no change). So the
//! compaction is its own call, `claude -p /compact --resume <id>`, without that
//! flag and from the session's folder, where Claude Code keeps the session. The
//! session keeps its id, and its next turn continues the compacted conversation.
//! The call uses the login of the user who runs it.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::claude_session::{REAP_TIMEOUT, allowlisted_with, kill_group};

/// How long a compaction may run; it asks the model for a summary of the whole conversation.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_MESSAGE_CHARS: usize = 200;
const COMPACT_PROMPT: &str = "/compact";
const IS_ERROR_FIELD: &str = "is_error";
const MESSAGE_FIELD: &str = "result";
/// `--setting-sources project` and `--strict-mcp-config` keep the user's hooks and
/// MCP servers out of the call, as they are out of a lean session.
const FIXED_ARGS: &[&str] = &[
    "-p",
    COMPACT_PROMPT,
    "--output-format",
    "json",
    "--setting-sources",
    "project",
    "--strict-mcp-config",
];
const RESUME_FLAG: &str = "--resume";

/// What to compact and how to run the call.
#[derive(Debug, Clone)]
pub struct CompactRequest<'a> {
    /// The Claude Code executable.
    pub binary: &'a Path,
    /// The parent environment; only allowlisted names and `env_passthrough` reach the child.
    pub parent_env: &'a [(OsString, OsString)],
    /// Names from `providers.claude_code.env_passthrough`.
    pub env_passthrough: &'a [String],
    /// The session's working folder, where Claude Code keeps its conversation.
    pub cwd: &'a Path,
    /// Claude Code's own id of the conversation, as `--resume` takes it.
    pub backend_session: &'a str,
    /// When to give up; see [`DEFAULT_TIMEOUT`].
    pub timeout: Duration,
}

/// Why a compaction did not happen.
#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    /// The executable could not be started.
    #[error("could not start {}: {source}", .path.display())]
    Spawn {
        /// The binary that was tried.
        path: PathBuf,
        /// The operating system error.
        source: io::Error,
    },
    /// Reading the output failed.
    #[error("the compaction failed: {0}")]
    Io(#[from] io::Error),
    /// The call took longer than the timeout and was stopped.
    #[error("claude did not finish compacting within {0:?}")]
    TimedOut(Duration),
    /// The child ended with a failure and printed no JSON result.
    #[error("claude exited with {0:?} and printed no result")]
    Exited(Option<i32>),
    /// The child printed something that is not one JSON object.
    #[error("claude printed something that is not a JSON result")]
    NotJson,
    /// The child said the call failed.
    #[error("claude could not compact{}", message_suffix(.0.as_deref()))]
    Reported(Option<String>),
    /// The child printed more than the limit.
    #[error("claude printed more than {0} bytes")]
    TooLarge(usize),
}

fn message_suffix(message: Option<&str>) -> String {
    message.map_or_else(String::new, |text| format!(": {text}"))
}

/// Stops the child's whole process group when the call ends, however it ends.
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_group(pid);
        }
    }
}

/// Compacts the conversation.
///
/// # Errors
/// Returns a [`CompactError`] when the binary cannot start, runs past the timeout,
/// prints too much or something other than a JSON result, or reports an error.
pub async fn compact(request: &CompactRequest<'_>) -> Result<(), CompactError> {
    let mut command = Command::new(request.binary);
    command
        .args(FIXED_ARGS)
        .args([RESUME_FLAG, request.backend_session])
        .env_clear()
        .envs(allowlisted_with(
            request.parent_env,
            request.env_passthrough,
        ))
        .current_dir(request.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().map_err(|source| CompactError::Spawn {
        path: request.binary.to_path_buf(),
        source,
    })?;
    let _group = GroupGuard(child.id());
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let run = async {
        let mut kept = Vec::new();
        // One byte past the limit shows that the child printed too much.
        (&mut stdout)
            .take(MAX_OUTPUT_BYTES as u64 + 1)
            .read_to_end(&mut kept)
            .await?;
        if kept.len() > MAX_OUTPUT_BYTES {
            // The child may still be writing, so there is no status to wait for.
            return Ok::<_, io::Error>((None, kept));
        }
        let status = child.wait().await?;
        Ok((Some(status), kept))
    };
    let Ok(done) = timeout(request.timeout, run).await else {
        if let Some(pid) = child.id() {
            kill_group(pid);
        }
        let _ = child.start_kill();
        let _ = timeout(REAP_TIMEOUT, child.wait()).await;
        return Err(CompactError::TimedOut(request.timeout));
    };
    let (status, kept) = done?;
    let Some(status) = status else {
        return Err(CompactError::TooLarge(MAX_OUTPUT_BYTES));
    };
    answer_of(&kept, status.success(), status.code())
}

fn answer_of(stdout: &[u8], success: bool, code: Option<i32>) -> Result<(), CompactError> {
    let Ok(result) = serde_json::from_slice::<Value>(stdout) else {
        return Err(if success {
            CompactError::NotJson
        } else {
            CompactError::Exited(code)
        });
    };
    if !result.is_object() {
        return Err(CompactError::NotJson);
    }
    if result[IS_ERROR_FIELD] == true {
        return Err(CompactError::Reported(
            result[MESSAGE_FIELD]
                .as_str()
                .map(|text| text.chars().take(MAX_MESSAGE_CHARS).collect()),
        ));
    }
    if success {
        Ok(())
    } else {
        Err(CompactError::Exited(code))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const OK_JSON: &str = r#"{"is_error":false,"result":""}"#;

    fn script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("claude");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn run(binary: &Path, cwd: &Path, wait: Duration) -> Result<(), CompactError> {
        compact(&CompactRequest {
            binary,
            parent_env: &[],
            env_passthrough: &[],
            cwd,
            backend_session: "abc-123",
            timeout: wait,
        })
        .await
    }

    #[tokio::test]
    async fn the_call_resumes_the_session_from_its_folder_without_the_slash_flag() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let log = dir.path().join("args");
        let binary = script(
            dir.path(),
            &format!(
                "echo \"$@\" > '{}'\npwd >> '{}'\necho '{OK_JSON}'",
                log.display(),
                log.display()
            ),
        );

        // Act
        let done = run(&binary, work.path(), DEFAULT_TIMEOUT).await;

        // Assert
        assert!(done.is_ok(), "{done:?}");
        let seen = fs::read_to_string(&log).unwrap();
        let mut lines = seen.lines();
        let args = lines.next().unwrap();
        assert_eq!(
            args,
            "-p /compact --output-format json --setting-sources project --strict-mcp-config --resume abc-123"
        );
        assert!(!args.contains("--disable-slash-commands"));
        let folder = fs::canonicalize(work.path()).unwrap();
        assert_eq!(fs::canonicalize(lines.next().unwrap()).unwrap(), folder);
    }

    #[tokio::test]
    async fn a_reported_error_a_failed_exit_and_odd_output_are_errors() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "echo '{\"is_error\":true,\"result\":\"Not enough messages to compact.\"}'",
                "Not enough messages",
            ),
            ("echo nonsense", "not a JSON result"),
            ("echo '[1]'", "not a JSON result"),
            ("exit 3", "exited with Some(3)"),
            ("echo '{\"is_error\":false}'; exit 1", "exited with Some(1)"),
        ];
        for (body, expected) in cases {
            let binary = script(dir.path(), body);

            // Act
            let failed = run(&binary, dir.path(), DEFAULT_TIMEOUT).await.unwrap_err();

            // Assert
            assert!(failed.to_string().contains(expected), "{body}: {failed}");
        }
    }

    #[tokio::test]
    async fn a_call_that_runs_too_long_is_stopped() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let binary = script(dir.path(), "sleep 30");

        // Act
        let failed = run(&binary, dir.path(), Duration::from_millis(200))
            .await
            .unwrap_err();

        // Assert
        assert!(matches!(failed, CompactError::TimedOut(_)), "{failed}");
    }

    #[tokio::test]
    async fn output_past_the_limit_is_refused() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let binary = script(dir.path(), "head -c 1200000 /dev/zero");

        // Act
        let failed = run(&binary, dir.path(), DEFAULT_TIMEOUT).await.unwrap_err();

        // Assert
        assert!(matches!(failed, CompactError::TooLarge(_)), "{failed}");
    }

    #[tokio::test]
    async fn a_missing_binary_is_a_spawn_error() {
        // Act
        let failed = run(
            Path::new("/no/such/claude"),
            Path::new("/"),
            DEFAULT_TIMEOUT,
        )
        .await
        .unwrap_err();

        // Assert
        assert!(matches!(failed, CompactError::Spawn { .. }), "{failed}");
    }
}
