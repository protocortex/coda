// SPDX-License-Identifier: Apache-2.0

//! The `git_status` and `git_diff` tools: read-only views of the working tree.
//!
//! Both run the `git` binary with a cleared environment and fixed arguments.
//! A repository's own configuration can name programs that git would run while
//! it answers (a file system monitor, an external diff, a text conversion
//! filter), so every call switches those off. No argument of the model reaches
//! git except a path, which is checked and passed as a literal path.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use crate::{Tool, ToolError, ToolOutput, parse_args};

pub const STATUS_NAME: &str = "git_status";
pub const DIFF_NAME: &str = "git_diff";
const GIT: &str = "git";
const REPOSITORY_MARKER: &str = ".git";
/// Switches before the subcommand: no pager, paths taken literally (so no
/// `:(...)` magic), and a file system monitor command from the repository
/// configuration is never run.
const GLOBAL_ARGS: [&str; 7] = [
    "--no-pager",
    "--literal-pathspecs",
    "--no-optional-locks",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "status.relativePaths=true",
];
/// Variables set on top of the caller's, so git never waits for a person, never
/// reads system wide configuration and never writes the index to refresh it.
const FIXED_ENV: [(&str, &str); 4] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_PAGER", "cat"),
];
/// Most bytes of output a tool returns. It is far above the usual tool cap, so
/// the loop can shorten a long diff and keep the whole of it behind a retrieve id.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Keys of the filter settings that name a program git runs on file content.
const FILTER_KEY_PREFIX: &str = "filter.";
const FILTER_PROGRAM_VARS: [&str; 3] = ["clean", "smudge", "process"];
/// What `git config --get-regexp` exits with when nothing matches.
const NO_MATCH_EXIT: i32 = 1;
/// How long to wait for git's error text once it has exited.
const STDERR_GRACE: Duration = Duration::from_millis(500);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How much of git's standard error is kept for a failure message.
const STDERR_KEEP_BYTES: usize = 4 * 1024;
const READ_CHUNK_BYTES: usize = 8 * 1024;
const PATH_DESCRIPTION_HINT: &str = "a path relative to the workspace";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffArgs {
    #[serde(default)]
    staged: bool,
    path: Option<String>,
}

/// Runs git in the workspace with the guards above.
#[derive(Debug, Clone)]
struct GitRunner {
    workdir: PathBuf,
    env: Vec<(OsString, OsString)>,
    timeout: Duration,
    max_output_bytes: usize,
}

impl GitRunner {
    fn command(&self) -> Command {
        let mut command = Command::new(GIT);
        command
            .args(GLOBAL_ARGS)
            .current_dir(&self.workdir)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .envs(FIXED_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// `-c` switches that empty every filter program the repository configures. A
    /// clean filter runs whenever git compares a file with the index, and
    /// `.gitattributes` in the tree picks which files it applies to.
    fn filter_overrides(&self) -> Result<Vec<String>, ToolError> {
        let listed = self
            .command()
            .args(["config", "--null", "--get-regexp", "^filter\\."])
            .output()
            .map_err(ToolError::Spawn)?;
        match listed.status.code() {
            Some(0) => {}
            Some(NO_MATCH_EXIT) => return Ok(Vec::new()),
            _ => {
                return Err(ToolError::Spawn(std::io::Error::other(format!(
                    "reading the git configuration failed: {}",
                    String::from_utf8_lossy(&listed.stderr).trim()
                ))));
            }
        }
        let mut names: Vec<String> = String::from_utf8_lossy(&listed.stdout)
            .split('\0')
            .filter_map(|entry| entry.split('\n').next())
            .filter_map(|key| key.strip_prefix(FILTER_KEY_PREFIX))
            .filter_map(|rest| rest.rsplit_once('.'))
            .map(|(name, _)| name.to_owned())
            .collect();
        names.sort();
        names.dedup();
        let mut switches = Vec::new();
        for name in names {
            for var in FILTER_PROGRAM_VARS {
                switches.push("-c".to_owned());
                switches.push(format!("{FILTER_KEY_PREFIX}{name}.{var}="));
            }
            switches.push("-c".to_owned());
            switches.push(format!("{FILTER_KEY_PREFIX}{name}.required=false"));
        }
        Ok(switches)
    }

    fn run(&self, args: &[&str], cancel: &AtomicBool) -> Result<ToolOutput, ToolError> {
        let overrides = self.filter_overrides()?;
        let mut child = self
            .command()
            .args(&overrides)
            .args(args)
            .spawn()
            .map_err(ToolError::Spawn)?;
        let stdout = child
            .stdout
            .take()
            .map(|pipe| spawn_reader(pipe, self.max_output_bytes + 1, true));
        let stderr = child
            .stderr
            .take()
            .map(|pipe| spawn_reader(pipe, STDERR_KEEP_BYTES, false));
        let deadline = Instant::now() + self.timeout;
        let Some(stdout) = stdout else {
            return Err(ToolError::Spawn(std::io::Error::other(
                "no standard output",
            )));
        };
        let waited = wait_for(&stdout, deadline, cancel);
        let Some(bytes) = waited else {
            return Ok(stopped(&mut child, cancel, self.timeout));
        };
        let over_cap = bytes.len() > self.max_output_bytes;
        if over_cap {
            // The pipe may be full, so a command with more to say would never exit.
            let _ = child.kill();
        }
        let status = child.wait().map_err(ToolError::Spawn)?;
        let mut output = ToolOutput::from_bytes(bytes, self.max_output_bytes);
        if over_cap {
            let _ = write!(
                output.text,
                "\n[cut at {} bytes; pass a path to see a smaller part]",
                self.max_output_bytes
            );
        } else if !status.success() {
            let detail = stderr
                .and_then(|reader| reader.recv_timeout(STDERR_GRACE).ok())
                .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
                .unwrap_or_default();
            let code = status
                .code()
                .map_or_else(|| "a signal".to_owned(), |c| c.to_string());
            if !output.text.is_empty() && !output.text.ends_with('\n') {
                output.text.push('\n');
            }
            let _ = write!(output.text, "[git exited with {code}] {detail}");
        }
        Ok(output)
    }
}

/// Keeps the first `limit` bytes of `pipe`, read on its own thread so a command
/// that fills one pipe never blocks on the other. With `stop_at_limit` the
/// reader quits once it holds `limit` bytes; otherwise it drains the pipe.
fn spawn_reader(
    mut pipe: impl Read + Send + 'static,
    limit: usize,
    stop_at_limit: bool,
) -> Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = [0_u8; READ_CHUNK_BYTES];
        loop {
            let read = match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let room = limit.saturating_sub(kept.len());
            kept.extend_from_slice(&chunk[..read.min(room)]);
            if stop_at_limit && kept.len() >= limit {
                break;
            }
        }
        let _ = sender.send(kept);
    });
    receiver
}

/// The bytes the reader gives, or `None` when the deadline passed or the call was cancelled.
fn wait_for(reader: &Receiver<Vec<u8>>, deadline: Instant, cancel: &AtomicBool) -> Option<Vec<u8>> {
    loop {
        if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return None;
        }
        match reader.recv_timeout(POLL_INTERVAL) {
            Ok(bytes) => return Some(bytes),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Some(Vec::new()),
        }
    }
}

fn stopped(child: &mut Child, cancel: &AtomicBool, timeout: Duration) -> ToolOutput {
    let _ = child.kill();
    let _ = child.wait();
    let text = if cancel.load(Ordering::Relaxed) {
        "[cancelled, git killed]".to_owned()
    } else {
        format!("[timed out after {} ms, git killed]", timeout.as_millis())
    };
    ToolOutput {
        text,
        truncated: false,
    }
}

/// Whether `dir` is inside a working tree: it or a parent holds a `.git` entry.
/// The tools are offered only then, so a folder with no history pays for no schema.
#[must_use]
pub fn in_repository(dir: &Path) -> bool {
    dir.ancestors()
        .any(|folder| folder.join(REPOSITORY_MARKER).exists())
}

/// A path the model gave, if it stays inside the workspace.
fn checked_path(path: &str) -> Result<&str, ToolError> {
    let reason = if path.is_empty() || path.contains('\0') {
        Some("the path is empty or holds a NUL byte")
    } else if Path::new(path).is_absolute() {
        Some("the path must be relative to the workspace")
    } else if Path::new(path)
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        Some("the path must not climb out of the workspace")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(ToolError::InvalidInput {
            tool: DIFF_NAME,
            reason: format!("{reason}; give {PATH_DESCRIPTION_HINT}"),
        }),
        None => Ok(path),
    }
}

/// Shows the branch and the changed and untracked files of the workspace.
#[derive(Debug, Clone)]
pub struct GitStatusTool(GitRunner);

/// Shows the changes of the workspace as a unified diff.
#[derive(Debug, Clone)]
pub struct GitDiffTool(GitRunner);

impl GitStatusTool {
    /// Creates the tool. `env` is the only environment git sees.
    #[must_use]
    pub fn new(
        workdir: PathBuf,
        env: Vec<(OsString, OsString)>,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Self {
        Self(GitRunner {
            workdir,
            env,
            timeout,
            max_output_bytes,
        })
    }
}

impl GitDiffTool {
    /// Creates the tool. `env` is the only environment git sees.
    #[must_use]
    pub fn new(
        workdir: PathBuf,
        env: Vec<(OsString, OsString)>,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Self {
        Self(GitRunner {
            workdir,
            env,
            timeout,
            max_output_bytes,
        })
    }
}

impl Tool for GitStatusTool {
    fn name(&self) -> &'static str {
        STATUS_NAME
    }

    fn read_only(&self) -> bool {
        true
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        self.call_cancellable(input, &AtomicBool::new(false))
    }

    fn call_cancellable(
        &self,
        input: &Value,
        cancel: &AtomicBool,
    ) -> Result<ToolOutput, ToolError> {
        let StatusArgs {} = parse_args(STATUS_NAME, input)?;
        self.0.run(
            &[
                "status",
                "--short",
                "--branch",
                "--ignore-submodules=all",
                "--",
                ".",
            ],
            cancel,
        )
    }
}

impl Tool for GitDiffTool {
    fn name(&self) -> &'static str {
        DIFF_NAME
    }

    fn read_only(&self) -> bool {
        true
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        self.call_cancellable(input, &AtomicBool::new(false))
    }

    fn call_cancellable(
        &self,
        input: &Value,
        cancel: &AtomicBool,
    ) -> Result<ToolOutput, ToolError> {
        let args: DiffArgs = parse_args(DIFF_NAME, input)?;
        let mut git = vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--relative",
            "--ignore-submodules=all",
        ];
        if args.staged {
            git.push("--cached");
        }
        if let Some(path) = &args.path {
            git.push("--");
            git.push(checked_path(path)?);
        }
        self.0.run(&git, cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    const CAP: usize = 64 * 1024;
    const TIMEOUT: Duration = Duration::from_secs(30);

    struct Repo {
        dir: tempfile::TempDir,
    }

    impl Repo {
        fn new() -> Self {
            let repo = Self {
                dir: tempfile::tempdir().unwrap(),
            };
            repo.git(&["init", "-q"]);
            repo
        }

        fn env(&self) -> Vec<(OsString, OsString)> {
            vec![
                (
                    OsString::from("PATH"),
                    std::env::var_os("PATH").unwrap_or_default(),
                ),
                (OsString::from("HOME"), self.dir.path().into()),
            ]
        }

        fn git(&self, args: &[&str]) {
            let status = Command::new(GIT)
                .args(args)
                .current_dir(self.dir.path())
                .env("HOME", self.dir.path())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }

        fn write(&self, name: &str, text: &str) {
            fs::write(self.dir.path().join(name), text).unwrap();
        }

        fn status(&self) -> GitStatusTool {
            GitStatusTool::new(self.dir.path().to_path_buf(), self.env(), TIMEOUT, CAP)
        }

        fn diff(&self) -> GitDiffTool {
            GitDiffTool::new(self.dir.path().to_path_buf(), self.env(), TIMEOUT, CAP)
        }
    }

    #[test]
    fn status_names_a_changed_file_and_an_untracked_one() {
        // Arrange
        let repo = Repo::new();
        repo.write("tracked.txt", "one\n");
        repo.git(&["add", "tracked.txt"]);
        repo.write("tracked.txt", "two\n");
        repo.write("new.txt", "fresh\n");

        // Act
        let out = repo.status().call(&json!({})).unwrap();

        // Assert
        assert!(out.text.contains("tracked.txt"), "{}", out.text);
        assert!(out.text.contains("?? new.txt"), "{}", out.text);
    }

    #[test]
    fn diff_shows_a_change_not_yet_staged() {
        // Arrange
        let repo = Repo::new();
        repo.write("a.txt", "one\n");
        repo.git(&["add", "a.txt"]);
        repo.write("a.txt", "two\n");

        // Act
        let out = repo.diff().call(&json!({})).unwrap();

        // Assert
        assert!(out.text.contains("-one"), "{}", out.text);
        assert!(out.text.contains("+two"), "{}", out.text);
    }

    #[test]
    fn diff_with_staged_shows_what_is_in_the_index() {
        // Arrange
        let repo = Repo::new();
        repo.write("a.txt", "staged line\n");
        repo.git(&["add", "a.txt"]);

        // Act
        let unstaged = repo.diff().call(&json!({})).unwrap();
        let staged = repo.diff().call(&json!({"staged": true})).unwrap();

        // Assert
        assert!(unstaged.text.is_empty(), "{}", unstaged.text);
        assert!(staged.text.contains("+staged line"), "{}", staged.text);
    }

    #[test]
    fn diff_with_a_path_shows_only_that_file() {
        // Arrange
        let repo = Repo::new();
        repo.write("a.txt", "one\n");
        repo.write("b.txt", "one\n");
        repo.git(&["add", "."]);
        repo.write("a.txt", "two\n");
        repo.write("b.txt", "two\n");

        // Act
        let out = repo.diff().call(&json!({"path": "b.txt"})).unwrap();

        // Assert
        assert!(out.text.contains("b.txt"), "{}", out.text);
        assert!(!out.text.contains("a.txt"), "{}", out.text);
    }

    #[test]
    fn diff_refuses_a_path_outside_the_workspace() {
        // Arrange
        let repo = Repo::new();

        for path in ["../elsewhere", "/etc/passwd", "", "a\0b"] {
            // Act
            let result = repo.diff().call(&json!({"path": path}));

            // Assert
            assert!(
                matches!(result, Err(ToolError::InvalidInput { .. })),
                "{path:?}: {result:?}"
            );
        }
    }

    #[test]
    fn diff_takes_a_path_literally_so_pathspec_magic_matches_nothing() {
        // Arrange
        let repo = Repo::new();
        repo.write("a.txt", "one\n");
        repo.git(&["add", "a.txt"]);
        repo.write("a.txt", "two\n");

        // Act
        let out = repo.diff().call(&json!({"path": ":(top)a.txt"})).unwrap();

        // Assert
        assert!(out.text.is_empty(), "{}", out.text);
    }

    #[test]
    fn an_argument_the_tool_does_not_know_is_refused() {
        // Arrange
        let repo = Repo::new();

        // Act
        let result = repo.diff().call(&json!({"extra": "--output=/tmp/x"}));

        // Assert
        assert!(matches!(result, Err(ToolError::InvalidInput { .. })));
    }

    #[test]
    fn a_long_diff_is_cut_at_the_cap_and_flagged() {
        // Arrange
        let repo = Repo::new();
        repo.write("a.txt", "x\n");
        repo.git(&["add", "a.txt"]);
        repo.write("a.txt", &"y\n".repeat(500));
        let tool = GitDiffTool::new(repo.dir.path().to_path_buf(), repo.env(), TIMEOUT, 100);

        // Act
        let out = tool.call(&json!({})).unwrap();

        // Assert
        assert!(out.truncated);
        assert!(out.text.contains("[cut at 100 bytes"), "{}", out.text);
    }

    #[test]
    fn a_folder_that_is_not_a_repository_gets_git_s_message() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let env = vec![
            (
                OsString::from("PATH"),
                std::env::var_os("PATH").unwrap_or_default(),
            ),
            (OsString::from("GIT_CEILING_DIRECTORIES"), "/".into()),
        ];
        let tool = GitStatusTool::new(dir.path().to_path_buf(), env, TIMEOUT, CAP);

        // Act
        let out = tool.call(&json!({})).unwrap();

        // Assert
        assert!(out.text.contains("not a git repository"), "{}", out.text);
    }

    #[test]
    fn programs_named_in_the_repository_config_are_never_run() {
        // Arrange
        let repo = Repo::new();
        let marker = repo.dir.path().join("ran");
        let touch = format!("touch {}", marker.display());
        repo.write(".gitattributes", "*.txt diff=evil\n");
        repo.write("a.txt", "one\n");
        repo.git(&["add", "."]);
        repo.write("a.txt", "two\n");
        repo.git(&["config", "diff.external", &touch]);
        repo.git(&["config", "diff.evil.textconv", &touch]);
        repo.git(&["config", "core.fsmonitor", &touch]);

        // Act
        repo.status().call(&json!({})).unwrap();
        repo.diff().call(&json!({})).unwrap();
        repo.diff().call(&json!({"staged": true})).unwrap();

        // Assert
        assert!(
            !marker.exists(),
            "git ran a program from the repository config"
        );
    }

    #[test]
    fn a_clean_filter_named_in_the_repository_config_is_never_run() {
        // Arrange
        let repo = Repo::new();
        let marker = repo.dir.path().join("ran");
        let touch = format!("touch {}", marker.display());
        repo.write(".gitattributes", "*.txt filter=evil\n");
        repo.write("a.txt", "one\n");
        repo.git(&["add", "."]);
        repo.write("a.txt", "two\n");
        repo.git(&["config", "filter.evil.clean", &touch]);
        repo.git(&["config", "filter.evil.smudge", &touch]);

        // Act
        repo.status().call(&json!({})).unwrap();
        repo.diff().call(&json!({})).unwrap();

        // Assert
        assert!(
            !marker.exists(),
            "git ran a filter from the repository config"
        );
    }

    #[test]
    fn a_workspace_below_the_repository_root_sees_only_its_own_changes() {
        // Arrange
        let repo = Repo::new();
        let inner = repo.dir.path().join("sub");
        fs::create_dir(&inner).unwrap();
        repo.write("outside.txt", "one\n");
        repo.write("sub/inside.txt", "one\n");
        repo.git(&["add", "."]);
        repo.write("outside.txt", "secret change\n");
        repo.write("sub/inside.txt", "two\n");
        repo.write("sub/new.txt", "fresh\n");
        repo.write("untracked-outside.txt", "fresh\n");
        let status = GitStatusTool::new(inner.clone(), repo.env(), TIMEOUT, CAP);
        let diff = GitDiffTool::new(inner, repo.env(), TIMEOUT, CAP);

        // Act
        let listed = status.call(&json!({})).unwrap();
        let shown = diff.call(&json!({})).unwrap();
        let one = diff.call(&json!({"path": "inside.txt"})).unwrap();

        // Assert
        assert!(listed.text.contains("inside.txt"), "{}", listed.text);
        assert!(listed.text.contains("new.txt"), "{}", listed.text);
        assert!(!listed.text.contains("outside"), "{}", listed.text);
        assert!(shown.text.contains("+two"), "{}", shown.text);
        assert!(!shown.text.contains("secret"), "{}", shown.text);
        assert!(one.text.contains("+two"), "{}", one.text);
    }

    #[test]
    fn a_command_over_its_time_limit_is_killed_and_reported() {
        // Arrange
        let repo = Repo::new();
        let tool = GitStatusTool::new(
            repo.dir.path().to_path_buf(),
            repo.env(),
            Duration::ZERO,
            CAP,
        );

        // Act
        let out = tool.call(&json!({})).unwrap();

        // Assert
        assert!(out.text.contains("timed out"), "{}", out.text);
    }

    #[test]
    fn a_cancelled_call_stops_git_and_says_so() {
        // Arrange
        let repo = Repo::new();
        let cancel = AtomicBool::new(true);

        // Act
        let out = repo.status().call_cancellable(&json!({}), &cancel).unwrap();

        // Assert
        assert!(out.text.contains("cancelled"), "{}", out.text);
    }

    #[test]
    fn a_folder_is_in_a_repository_when_it_or_a_parent_has_a_marker() {
        // Arrange
        let repo = Repo::new();
        let nested = repo.dir.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        let bare = tempfile::tempdir().unwrap();

        // Act / Assert
        assert!(in_repository(repo.dir.path()));
        assert!(in_repository(&nested));
        assert!(!in_repository(bare.path()));
    }

    #[test]
    fn both_tools_are_read_only_for_the_loop() {
        // Arrange
        let repo = Repo::new();

        // Act / Assert
        assert!(repo.status().read_only());
        assert!(repo.diff().read_only());
    }
}
