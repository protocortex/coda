// SPDX-License-Identifier: AGPL-3.0-only

//! The `bash` tool: one shell command with a timeout, an output cap and a
//! cleared environment.

use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(not(target_vendor = "apple"))]
use rustix::pipe::{PipeFlags, pipe_with};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use serde::Deserialize;
use serde_json::Value;

use crate::{Tool, ToolError, ToolOutput, parse_args};

const NAME: &str = "bash";
const SHELL: &str = "/bin/sh";
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long to wait for the output reader after the command is gone, in case
/// a process that left the group still holds the pipe open.
const READER_GRACE: Duration = Duration::from_millis(500);
const READ_CHUNK_BYTES: usize = 8 * 1024;

/// Environment variables passed to commands unless the caller says otherwise.
pub const DEFAULT_ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TZ"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    timeout_ms: Option<u64>,
}

/// Limits and environment for [`BashTool`].
#[derive(Debug, Clone)]
pub struct BashConfig {
    /// The longest a command may run; a request may only ask for less.
    pub timeout: Duration,
    /// The most output bytes returned to the model.
    pub max_output_bytes: usize,
    /// The only environment variables the command sees.
    pub env: Vec<(OsString, OsString)>,
}

/// Keeps the variables of `vars` whose names are in `allowlist`.
#[must_use]
pub fn allowlisted_env(
    allowlist: &[&str],
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    vars.into_iter()
        .filter(|(name, _)| allowlist.iter().any(|allowed| name == allowed))
        .collect()
}

/// Runs a shell command in the workspace.
pub struct BashTool {
    workdir: PathBuf,
    config: BashConfig,
}

impl BashTool {
    /// Creates the tool; commands run with `workdir` as their directory.
    #[must_use]
    pub fn new(workdir: PathBuf, config: BashConfig) -> Self {
        Self { workdir, config }
    }
}

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        self.execute(input, &AtomicBool::new(false), Capture::start)
    }

    fn call_cancellable(
        &self,
        input: &Value,
        cancel: &AtomicBool,
    ) -> Result<ToolOutput, ToolError> {
        self.execute(input, cancel, Capture::start)
    }
}

impl BashTool {
    /// Runs the command; `start_capture` is a parameter so tests can make
    /// the output reader fail after the shell has started.
    fn execute(
        &self,
        input: &Value,
        cancel: &AtomicBool,
        start_capture: impl FnOnce(OwnedFd, usize) -> io::Result<Capture>,
    ) -> Result<ToolOutput, ToolError> {
        let args: BashArgs = parse_args(NAME, input)?;
        let timeout = args.timeout_ms.map_or(self.config.timeout, |ms| {
            Duration::from_millis(ms).min(self.config.timeout)
        });
        let (reader, writer) = cloexec_pipe().map_err(ToolError::Spawn)?;
        let child = self
            .spawn(&args.command, writer)
            .map_err(ToolError::Spawn)?;
        // Owned from here on, so a failure below still kills and reaps the shell.
        let mut running = RunningCommand::new(child);
        let capture =
            start_capture(reader, self.config.max_output_bytes).map_err(ToolError::Spawn)?;

        let outcome = running.wait(timeout, cancel).map_err(ToolError::Spawn)?;
        let captured = capture.finish();

        let mut output = ToolOutput::from_bytes(captured, self.config.max_output_bytes);
        output.text.push_str(&outcome.trailer(timeout));
        Ok(output)
    }
}

impl BashTool {
    /// Starts the shell in its own process group with both output streams on `writer`.
    fn spawn(&self, command: &str, writer: OwnedFd) -> io::Result<Child> {
        #[cfg(target_vendor = "apple")]
        let _guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let stderr = writer.try_clone()?;
        Command::new(SHELL)
            .arg("-c")
            .arg(command)
            .current_dir(&self.workdir)
            .env_clear()
            .envs(self.config.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(writer)
            .stderr(stderr)
            .process_group(0)
            .spawn()
    }
}

/// A pipe whose ends do not leak into unrelated children; the shell gets
/// its copy through `dup2` onto stdout and stderr. Both flags are set at
/// creation, so a concurrent spawn can never inherit an end.
#[cfg(not(target_vendor = "apple"))]
fn cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    Ok(pipe_with(PipeFlags::CLOEXEC)?)
}

/// Apple systems have no atomic close-on-exec pipe, so the flags are set right
/// after creation while holding the lock that every shell start also takes.
#[cfg(target_vendor = "apple")]
fn cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    use rustix::io::{FdFlags, fcntl_setfd};

    let _guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let (reader, writer) = rustix::pipe::pipe()?;
    fcntl_setfd(&reader, FdFlags::CLOEXEC)?;
    fcntl_setfd(&writer, FdFlags::CLOEXEC)?;
    Ok((reader, writer))
}

#[cfg(target_vendor = "apple")]
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

enum Outcome {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
}

impl Outcome {
    /// A line telling the model how the command ended, empty on success.
    fn trailer(&self, timeout: Duration) -> String {
        match self {
            Self::TimedOut => format!(
                "\n[timed out after {} ms, process group killed]",
                timeout.as_millis()
            ),
            Self::Cancelled => "\n[cancelled, process group killed]".to_owned(),
            Self::Exited(status) => match status.code() {
                Some(0) => String::new(),
                Some(code) => format!("\n[exit status {code}]"),
                None => "\n[terminated by signal]".to_owned(),
            },
        }
    }
}

/// A started shell. Dropping it kills the whole process group, so no early
/// return leaves a command running.
///
/// The group id is the shell's pid, which the system may hand to an unrelated
/// process once the shell is reaped. The group is therefore always killed
/// before the shell is reaped, and forgotten afterwards.
struct RunningCommand {
    child: Child,
    group: Option<Pid>,
}

impl RunningCommand {
    fn new(child: Child) -> Self {
        let group = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
        Self { child, group }
    }

    /// Waits for the shell to exit, killing its group once `timeout` passes
    /// or `cancel` is set.
    ///
    /// Either way the group is killed first, which also ends stragglers that
    /// would keep the output pipe open, and only then is the shell reaped.
    fn wait(&mut self, timeout: Duration, cancel: &AtomicBool) -> io::Result<Outcome> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.has_exited()? {
                self.kill_group();
                let status = self.child.wait()?;
                self.group = None;
                return Ok(Outcome::Exited(status));
            }
            let cancelled = cancel.load(Ordering::SeqCst);
            if cancelled || Instant::now() >= deadline {
                self.kill_group();
                self.child.wait()?;
                self.group = None;
                return Ok(if cancelled {
                    Outcome::Cancelled
                } else {
                    Outcome::TimedOut
                });
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Whether the shell has exited, without reaping it.
    fn has_exited(&self) -> io::Result<bool> {
        let Some(group) = self.group else {
            return Ok(true);
        };
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
        Ok(waitid(WaitId::Pid(group), options)?.is_some())
    }

    /// Sends SIGKILL to every process in the group; an empty group is fine.
    fn kill_group(&self) {
        if let Some(group) = self.group {
            let _ = kill_process_group(group, Signal::KILL);
        }
    }
}

impl Drop for RunningCommand {
    fn drop(&mut self) {
        self.kill_group();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
}

/// Collects the first `cap + 1` output bytes on a thread and discards the
/// rest, so a chatty command can neither block nor grow memory.
struct Capture {
    shared: Arc<Mutex<Captured>>,
    finished: Receiver<()>,
}

impl Capture {
    fn start(reader: OwnedFd, cap: usize) -> io::Result<Self> {
        let shared = Arc::new(Mutex::new(Captured::default()));
        let (done, finished) = mpsc::channel();
        let sink = Arc::clone(&shared);
        thread::Builder::new()
            .name("bash-output".into())
            .spawn(move || {
                drain(reader.into(), cap, &sink);
                let _ = done.send(());
            })?;
        Ok(Self { shared, finished })
    }

    /// Returns what was read, waiting a short grace for the reader to see EOF.
    fn finish(self) -> Vec<u8> {
        let _ = self.finished.recv_timeout(READER_GRACE);
        let mut guard = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut guard.bytes)
    }
}

fn drain(mut source: std::fs::File, cap: usize, sink: &Mutex<Captured>) {
    let keep = cap.saturating_add(1);
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    loop {
        match source.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => {
                let mut guard = sink.lock().unwrap_or_else(PoisonError::into_inner);
                let room = keep.saturating_sub(guard.bytes.len());
                guard.bytes.extend_from_slice(&chunk[..n.min(room)]);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use rustix::process::{Pid, test_kill_process};
    use serde_json::json;
    use std::time::Instant;
    use tempfile::TempDir;

    const CAP: usize = 16;
    const SHORT: Duration = Duration::from_millis(300);
    const POLL: Duration = Duration::from_millis(20);
    const BOUND: Duration = Duration::from_secs(10);

    fn tool(timeout: Duration, env: &[(&str, &str)]) -> (TempDir, BashTool) {
        tool_capped(timeout, env, CAP)
    }

    fn tool_capped(timeout: Duration, env: &[(&str, &str)], cap: usize) -> (TempDir, BashTool) {
        let dir = tempfile::tempdir().unwrap();
        let config = BashConfig {
            timeout,
            max_output_bytes: cap,
            env: env
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        };
        let tool = BashTool::new(dir.path().to_path_buf(), config);
        (dir, tool)
    }

    fn run(tool: &BashTool, command: &str) -> ToolOutput {
        tool.call(&json!({ "command": command })).unwrap()
    }

    #[test]
    fn captures_stdout() {
        // Arrange
        let (_dir, tool) = tool(BOUND, &[]);

        // Act
        let out = run(&tool, "echo hi");

        // Assert
        assert_eq!(out.text, "hi\n");
        assert!(!out.truncated);
    }

    #[test]
    fn merges_stderr_into_the_output() {
        // Arrange
        let (_dir, tool) = tool(BOUND, &[]);

        // Act
        let out = run(&tool, "echo oops >&2");

        // Assert
        assert_eq!(out.text, "oops\n");
    }

    #[test]
    fn nonzero_exit_is_reported() {
        // Arrange
        let (_dir, tool) = tool(BOUND, &[]);

        // Act
        let out = run(&tool, "echo out; exit 3");

        // Assert
        assert_eq!(out.text, "out\n\n[exit status 3]");
    }

    #[test]
    fn runs_in_the_workdir() {
        // Arrange
        let (dir, tool) = tool(BOUND, &[]);
        let expected = dir.path().file_name().unwrap().to_str().unwrap().to_owned();

        // Act
        let out = run(&tool, "basename \"$(pwd)\"");

        // Assert
        assert_eq!(out.text.trim_end(), expected);
    }

    #[test]
    fn only_allowlisted_environment_is_visible() {
        // Arrange
        let (_dir, tool) = tool_capped(BOUND, &[("CODA_ALLOWED", "1")], 4096);

        // Act
        let out = tool.call(&json!({"command": "/usr/bin/env"})).unwrap().text;

        // Assert
        assert!(out.lines().any(|l| l == "CODA_ALLOWED=1"), "{out}");
        assert!(!out.lines().any(|l| l.starts_with("HOME=")), "{out}");
    }

    #[test]
    fn allowlisted_env_drops_unlisted_names() {
        // Arrange
        let vars = [("PATH", "/bin"), ("SECRET_TOKEN", "x"), ("HOME", "/h")]
            .map(|(k, v)| (OsString::from(k), OsString::from(v)));

        // Act
        let kept = allowlisted_env(DEFAULT_ENV_ALLOWLIST, vars);

        // Assert
        let names: Vec<_> = kept.iter().map(|(k, _)| k.to_str().unwrap()).collect();
        assert_eq!(names, ["PATH", "HOME"]);
    }

    #[test]
    fn stdin_is_closed() {
        // Arrange
        let (_dir, tool) = tool(BOUND, &[]);

        // Act
        let out = run(&tool, "cat");

        // Assert
        assert_eq!(out.text, "");
    }

    #[test]
    fn output_over_the_cap_is_cut_and_flagged() {
        // Arrange
        let (_dir, tool) = tool(BOUND, &[]);

        // Act
        let out = run(&tool, "head -c 100000 /dev/zero | tr '\\0' a");

        // Assert
        assert_eq!(out.text, "a".repeat(CAP));
        assert!(out.truncated);
    }

    #[test]
    fn endless_output_is_bounded_by_the_timeout() {
        // Arrange
        let (_dir, tool) = tool(SHORT, &[]);
        let started = Instant::now();

        // Act
        let out = run(&tool, "yes");

        // Assert
        assert!(started.elapsed() < BOUND);
        assert!(out.truncated);
        assert!(out.text.starts_with("y\ny\n"));
        assert!(
            out.text
                .ends_with("[timed out after 300 ms, process group killed]")
        );
    }

    #[test]
    fn timeout_stops_a_long_command() {
        // Arrange
        let (_dir, tool) = tool(SHORT, &[]);
        let started = Instant::now();

        // Act
        let out = run(&tool, "sleep 60");

        // Assert
        assert!(started.elapsed() < BOUND);
        assert_eq!(out.text, "\n[timed out after 300 ms, process group killed]");
    }

    #[test]
    fn request_may_shorten_but_not_extend_the_timeout() {
        // Arrange
        let (_dir, tool) = tool(SHORT, &[]);
        let started = Instant::now();

        // Act
        let out = tool
            .call(&json!({"command": "sleep 60", "timeout_ms": 3_600_000}))
            .unwrap();

        // Assert
        assert!(started.elapsed() < BOUND);
        assert!(out.text.contains("timed out after 300 ms"));
    }

    #[test]
    fn timeout_kills_the_whole_process_group() {
        // Arrange
        let (_dir, tool) = tool(SHORT, &[]);

        // Act
        let out = run(&tool, "sleep 300 & echo $!; wait");

        // Assert
        let pid: i32 = out.text.lines().next().unwrap().parse().unwrap();
        let pid = Pid::from_raw(pid).unwrap();
        let deadline = Instant::now() + BOUND;
        while test_kill_process(pid).is_ok() {
            assert!(Instant::now() < deadline, "grandchild {pid:?} survived");
            std::thread::sleep(POLL);
        }
    }

    #[test]
    fn command_blocked_on_a_fifo_is_killed_and_returns() {
        // Arrange
        let (_dir, tool) = tool(SHORT, &[]);
        let started = Instant::now();

        // Act
        let out = run(&tool, "mkfifo pipe && cat pipe");

        // Assert
        assert!(started.elapsed() < BOUND);
        assert!(out.text.contains("timed out"));
    }

    #[test]
    fn missing_command_argument_is_invalid_input() {
        // Arrange
        let (_dir, tool) = tool(BOUND, &[]);

        // Act
        let result = tool.call(&json!({}));

        // Assert
        assert!(matches!(result, Err(ToolError::InvalidInput { .. })));
    }

    #[test]
    fn stragglers_are_killed_when_the_shell_exits_on_its_own() {
        // Arrange
        let (_dir, tool) = tool(BOUND, &[]);

        // Act
        let out = run(&tool, "sleep 300 & echo $!");

        // Assert
        let pid: i32 = out.text.lines().next().unwrap().parse().unwrap();
        let pid = Pid::from_raw(pid).unwrap();
        let deadline = Instant::now() + BOUND;
        while test_kill_process(pid).is_ok() {
            assert!(Instant::now() < deadline, "straggler {pid:?} survived");
            std::thread::sleep(POLL);
        }
    }

    #[test]
    fn group_is_forgotten_once_the_shell_is_reaped() {
        // Arrange
        let (_dir, tool) = tool(BOUND, &[]);
        let (_reader, writer) = cloexec_pipe().unwrap();
        let child = tool.spawn("exit 0", writer).unwrap();
        let mut running = RunningCommand::new(child);

        // Act
        let outcome = running.wait(BOUND, &AtomicBool::new(false)).unwrap();

        // Assert
        assert!(matches!(outcome, Outcome::Exited(_)));
        assert!(running.group.is_none());
    }

    #[test]
    fn capture_failure_after_spawn_kills_and_reaps_the_shell() {
        // Arrange
        let (dir, tool) = tool(BOUND, &[]);
        let pid_file = dir.path().join("pid");
        let waiting_for = pid_file.clone();
        let failing = move |_reader: OwnedFd, _cap: usize| -> io::Result<Capture> {
            let deadline = Instant::now() + BOUND;
            while !std::fs::read_to_string(&waiting_for).is_ok_and(|s| s.ends_with('\n')) {
                assert!(Instant::now() < deadline, "shell never wrote its pid");
                std::thread::sleep(POLL);
            }
            Err(io::Error::other("injected"))
        };

        // Act
        let result = tool.execute(
            &json!({"command": "echo $$ > pid; sleep 300"}),
            &AtomicBool::new(false),
            failing,
        );

        // Assert
        assert!(matches!(result, Err(ToolError::Spawn(_))));
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(test_kill_process(Pid::from_raw(pid).unwrap()).is_err());
    }

    #[test]
    fn concurrent_calls_never_wait_on_a_stray_pipe_end() {
        // Arrange
        let (_dir, tool) = tool(BOUND, &[]);
        let tool = Arc::new(tool);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // A long-lived child that is spawned while pipes are being created
        // would inherit any end that is not close-on-exec.
        let spawner = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
                    thread::sleep(Duration::from_millis(2));
                    child.kill().unwrap();
                    child.wait().unwrap();
                }
            })
        };

        // Act
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let tool = Arc::clone(&tool);
                thread::spawn(move || {
                    (0..25)
                        .map(|_| {
                            let started = Instant::now();
                            run(&tool, "echo hi");
                            started.elapsed()
                        })
                        .max()
                        .unwrap()
                })
            })
            .collect();
        let slowest = workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .max()
            .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        spawner.join().unwrap();

        // Assert
        assert!(slowest < READER_GRACE, "a call took {slowest:?}");
    }
}
