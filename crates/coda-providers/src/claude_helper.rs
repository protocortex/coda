// SPDX-License-Identifier: Apache-2.0

//! One cheap call of the installed Claude Code that answers with JSON that fits a
//! schema, for short helper jobs such as a session title, a commit message or a
//! branch name.
//!
//! The call is `claude -p` with no tools, no MCP servers, no settings, no skills
//! and no saved session, run from an empty temporary folder so no project file
//! joins the prompt. Coda hands the prompt over on standard input and reads one
//! JSON result from standard output. It uses the login of the user who runs it.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::timeout;

use crate::claude_session::{REAP_TIMEOUT, allowlisted_with, kill_group};

/// How long a helper call may run before it is stopped.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

/// The most standard output Coda keeps; a title or a commit message is far smaller.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// The most characters of Claude Code's own error text that an error keeps.
const MAX_MESSAGE_CHARS: usize = 200;
const IS_ERROR_FIELD: &str = "is_error";
const SUBTYPE_FIELD: &str = "subtype";
const MESSAGE_FIELD: &str = "result";
const OUTPUT_FIELD: &str = "structured_output";
const UNKNOWN_SUBTYPE: &str = "unknown";
const SCHEMA_FLAG: &str = "--json-schema";
const MODEL_FLAG: &str = "--model";
/// The only prompt of the call; Claude Code's own is thousands of tokens longer.
const HELPER_SYSTEM_PROMPT: &str =
    "You write short plain text for a coding tool. Answer with the requested JSON only.";
/// Empty values are meant: `--tools ""` offers no tool and `--setting-sources ""` reads no
/// settings file, so no hook, memory file or allow rule joins the call.
const FIXED_ARGS: &[&str] = &[
    "-p",
    "--output-format",
    "json",
    "--tools",
    "",
    "--no-session-persistence",
    "--strict-mcp-config",
    "--disable-slash-commands",
    "--setting-sources",
    "",
    "--system-prompt",
    HELPER_SYSTEM_PROMPT,
];

/// What to ask and how to run the call.
#[derive(Debug, Clone)]
pub struct HelperRequest<'a> {
    /// The Claude Code executable.
    pub binary: &'a Path,
    /// The parent environment; only allowlisted names and `env_passthrough` reach the child.
    pub parent_env: &'a [(OsString, OsString)],
    /// Names from `providers.claude_code.env_passthrough`.
    pub env_passthrough: &'a [String],
    /// The text sent on standard input.
    pub prompt: &'a str,
    /// The JSON Schema the answer must fit.
    pub schema: &'a Value,
    /// The model to use; Claude Code's own default when unset.
    pub model: Option<&'a str>,
    /// When to give up; see [`DEFAULT_TIMEOUT`].
    pub timeout: Duration,
}

/// Why a helper call gave no answer.
#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    /// The executable could not be started.
    #[error("could not start {}: {source}", .path.display())]
    Spawn {
        /// The binary that was tried.
        path: PathBuf,
        /// The operating system error.
        source: io::Error,
    },
    /// Talking to the child or making its folder failed.
    #[error("the helper call failed: {0}")]
    Io(#[from] io::Error),
    /// The call took longer than the timeout and was stopped.
    #[error("claude gave no answer within {0:?}")]
    TimedOut(Duration),
    /// The child ended with a failure and printed no JSON result.
    #[error("claude exited with {0:?} and printed no result")]
    Exited(Option<i32>),
    /// The child printed something that is not one JSON object.
    #[error("claude printed something that is not a JSON result")]
    NotJson,
    /// The child said the call failed.
    #[error("claude reported an error ({subtype}){}", message_suffix(.message.as_deref()))]
    Reported {
        /// The `subtype` of the result.
        subtype: String,
        /// Claude Code's own words for what went wrong, cut short.
        message: Option<String>,
    },
    /// The result has no structured output.
    #[error("claude's result has no structured output")]
    NoStructuredOutput,
    /// The child printed more than the limit.
    #[error("claude printed more than {0} bytes")]
    TooLarge(usize),
}

fn message_suffix(message: Option<&str>) -> String {
    message.map_or_else(String::new, |text| format!(": {text}"))
}

/// Stops the child's whole process group when the call ends, however it ends,
/// including when the caller drops the future.
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_group(pid);
        }
    }
}

/// Runs the call and returns the structured output.
///
/// # Errors
/// Returns a [`HelperError`] when the binary cannot start, runs past the timeout,
/// prints too much or something other than a JSON result, or reports an error.
pub async fn run_structured(request: &HelperRequest<'_>) -> Result<Value, HelperError> {
    let folder = tempfile::tempdir()?;
    let mut command = Command::new(request.binary);
    command
        .args(helper_args(request))
        .env_clear()
        .envs(allowlisted_with(
            request.parent_env,
            request.env_passthrough,
        ))
        .current_dir(folder.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().map_err(|source| HelperError::Spawn {
        path: request.binary.to_path_buf(),
        source,
    })?;
    let group = GroupGuard(child.id());
    let exchange = exchange(&mut child, request.prompt);
    let Ok(done) = timeout(request.timeout, exchange).await else {
        stop(&mut child).await;
        return Err(HelperError::TimedOut(request.timeout));
    };
    let (status, stdout) = done?;
    let Some(status) = status else {
        stop(&mut child).await;
        return Err(HelperError::TooLarge(MAX_OUTPUT_BYTES));
    };
    drop(group);
    answer_of(&stdout, status)
}

/// Kills the child and its descendants and waits a bounded time for the child.
async fn stop(child: &mut Child) {
    if let Some(pid) = child.id() {
        kill_group(pid);
    }
    let _ = child.start_kill();
    let _ = timeout(REAP_TIMEOUT, child.wait()).await;
}

/// Writes the prompt, reads the whole answer and waits for the child.
async fn exchange(child: &mut Child, prompt: &str) -> io::Result<(Option<ExitStatus>, Vec<u8>)> {
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let write = async {
        if let Some(stdin) = stdin.as_mut() {
            // A child that exits early closes the pipe; its output says why.
            let _ = stdin.write_all(prompt.as_bytes()).await;
        }
        drop(stdin.take());
    };
    let read = async {
        let mut kept = Vec::new();
        // One byte past the limit shows that the child printed too much.
        (&mut stdout)
            .take(MAX_OUTPUT_BYTES as u64 + 1)
            .read_to_end(&mut kept)
            .await?;
        Ok::<_, io::Error>(kept)
    };
    tokio::pin!(write, read);
    let pid = child.id();
    let mut written = false;
    let mut status = None;
    // The read decides the end: a child that stops reading must not hold up an answer.
    let kept = loop {
        tokio::select! {
            () = &mut write, if !written => written = true,
            exited = child.wait(), if status.is_none() => {
                status = Some(exited?);
                // A descendant that kept the pipe open would hold the read until it ends.
                if let Some(pid) = pid {
                    kill_group(pid);
                }
            }
            kept = &mut read => break kept?,
        }
    };
    if kept.len() > MAX_OUTPUT_BYTES {
        // The child may still be writing, so there is no status to wait for.
        return Ok((None, kept));
    }
    let status = match status {
        Some(status) => status,
        None => child.wait().await?,
    };
    Ok((Some(status), kept))
}

fn answer_of(stdout: &[u8], status: ExitStatus) -> Result<Value, HelperError> {
    let Ok(result) = serde_json::from_slice::<Value>(stdout) else {
        return Err(if status.success() {
            HelperError::NotJson
        } else {
            HelperError::Exited(status.code())
        });
    };
    if !result.is_object() {
        return Err(HelperError::NotJson);
    }
    if result[IS_ERROR_FIELD] == true {
        return Err(HelperError::Reported {
            subtype: result[SUBTYPE_FIELD]
                .as_str()
                .unwrap_or(UNKNOWN_SUBTYPE)
                .to_owned(),
            message: result[MESSAGE_FIELD]
                .as_str()
                .map(|text| text.chars().take(MAX_MESSAGE_CHARS).collect()),
        });
    }
    if !status.success() {
        return Err(HelperError::Exited(status.code()));
    }
    match &result[OUTPUT_FIELD] {
        Value::Null => Err(HelperError::NoStructuredOutput),
        output => Ok(output.clone()),
    }
}

fn helper_args(request: &HelperRequest<'_>) -> Vec<String> {
    let mut args: Vec<String> = FIXED_ARGS.iter().map(|arg| (*arg).to_owned()).collect();
    args.extend([SCHEMA_FLAG.to_owned(), request.schema.to_string()]);
    if let Some(model) = request.model {
        args.extend([MODEL_FLAG.to_owned(), model.to_owned()]);
    }
    args
}
