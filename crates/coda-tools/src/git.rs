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
use std::sync::{Arc, OnceLock};
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
const FIXED_ENV: [(&str, &str); 5] = [
    // A missing object in a partial clone would run the remote's transport program.
    ("GIT_NO_LAZY_FETCH", "1"),
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
const REQUIRED_OFF: &str = "false";
/// The oldest git that honours every override below: `GIT_NO_LAZY_FETCH` came in
/// 2.44 and `GIT_CONFIG_COUNT` in 2.31. An older git ignores them without a message.
const MIN_GIT_VERSION: (u32, u32) = (2, 44);
const VERSION_ARG: &str = "--version";
const VERSION_PREFIX: &str = "git version ";
const CONFIG_COUNT_VAR: &str = "GIT_CONFIG_COUNT";
const CONFIG_PARAMETERS_VAR: &str = "GIT_CONFIG_PARAMETERS";
const CONFIG_KEY_VAR_PREFIX: &str = "GIT_CONFIG_KEY_";
const CONFIG_VALUE_VAR_PREFIX: &str = "GIT_CONFIG_VALUE_";
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
    /// The outcome of the first version check, shared by clones of the runner.
    version_check: Arc<OnceLock<Result<(), String>>>,
}

impl GitRunner {
    fn new(
        workdir: PathBuf,
        env: Vec<(OsString, OsString)>,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Self {
        Self {
            workdir,
            env,
            timeout,
            max_output_bytes,
            version_check: Arc::default(),
        }
    }

    /// Fails closed when git is older than the overrides need or says nothing
    /// parseable, because such a git would run a repository's programs unannounced.
    fn require_supported_git(&self) -> Result<(), ToolError> {
        let checked = self.version_check.get_or_init(|| {
            let output = self
                .bare_command()
                .arg(VERSION_ARG)
                .output()
                .map_err(|err| format!("could not run git --version: {err}"))?;
            let text = String::from_utf8_lossy(&output.stdout);
            match parse_git_version(&text) {
                Some(found) if found >= MIN_GIT_VERSION => Ok(()),
                Some((major, minor)) => Err(format!(
                    "git {major}.{minor} is too old: the git tools need git {}.{} or later, \
                     because an older git ignores the settings that stop a repository's programs from running",
                    MIN_GIT_VERSION.0, MIN_GIT_VERSION.1
                )),
                None => Err(format!(
                    "could not read the git version from {:?}; the git tools need git {}.{} or later",
                    text.trim(),
                    MIN_GIT_VERSION.0,
                    MIN_GIT_VERSION.1
                )),
            }
        });
        checked
            .clone()
            .map_err(|reason| ToolError::Spawn(std::io::Error::other(reason)))
    }

    fn command(&self) -> Command {
        let mut command = self.bare_command();
        command.args(GLOBAL_ARGS);
        command
    }

    /// Git with the guarded environment but no switches, so that the version
    /// probe works on a git too old to know the switches of `command`.
    fn bare_command(&self) -> Command {
        let mut command = Command::new(GIT);
        command
            .current_dir(&self.workdir)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .envs(FIXED_ENV)
            .env_remove(CONFIG_COUNT_VAR)
            .env_remove(CONFIG_PARAMETERS_VAR)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Key and value pairs that empty every filter program the repository
    /// configures. A clean filter runs whenever git compares a file with the
    /// index, and `.gitattributes` in the tree picks which files it applies to.
    /// They go in through `GIT_CONFIG_KEY_n`, because a `-c` switch splits at its
    /// first `=` and so cannot name a filter that holds one.
    fn filter_overrides(&self) -> Result<Vec<(String, String)>, ToolError> {
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
        let mut pairs = Vec::new();
        for name in names {
            for var in FILTER_PROGRAM_VARS {
                pairs.push((format!("{FILTER_KEY_PREFIX}{name}.{var}"), String::new()));
            }
            pairs.push((
                format!("{FILTER_KEY_PREFIX}{name}.required"),
                REQUIRED_OFF.to_owned(),
            ));
        }
        Ok(pairs)
    }

    fn run(&self, args: &[&str], cancel: &AtomicBool) -> Result<ToolOutput, ToolError> {
        self.require_supported_git()?;
        let overrides = self.filter_overrides()?;
        let mut command = self.command();
        command.env(CONFIG_COUNT_VAR, overrides.len().to_string());
        for (index, (key, value)) in overrides.iter().enumerate() {
            command.env(format!("{CONFIG_KEY_VAR_PREFIX}{index}"), key);
            command.env(format!("{CONFIG_VALUE_VAR_PREFIX}{index}"), value);
        }
        let mut child = command.args(args).spawn().map_err(ToolError::Spawn)?;
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

/// Reads `major.minor` from the text of `git --version`.
fn parse_git_version(text: &str) -> Option<(u32, u32)> {
    let numbers = text.trim().strip_prefix(VERSION_PREFIX)?;
    let mut parts = numbers.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
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
        Self(GitRunner::new(workdir, env, timeout, max_output_bytes))
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
        Self(GitRunner::new(workdir, env, timeout, max_output_bytes))
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
    /// Leading hex digits of an object id that name its folder under `.git/objects`.
    const OBJECT_FOLDER_LEN: usize = 2;
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
        repo.git(&["config", "diff.evil.command", &touch]);
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

    /// Filter names a `-c key=value` switch cannot carry whole, plus other odd ones.
    const ODD_FILTER_NAMES: [&str; 10] = [
        "a=b", "a==", "=", "A.B", "a..b", ".a", "a.", "a,b", "a\\b", "a:b",
    ];

    fn marker_after_calls_with_filter(name: &str, env: Vec<(OsString, OsString)>) -> bool {
        let repo = Repo::new();
        let marker = repo.dir.path().join("ran");
        let touch = format!("touch {}", marker.display());
        repo.write(".gitattributes", &format!("* filter={name}\n"));
        repo.write("a.txt", "one\n");
        repo.git(&["add", "."]);
        repo.write("a.txt", "two\n");
        for var in ["clean", "smudge", "process"] {
            repo.git(&["config", &format!("filter.{name}.{var}"), &touch]);
        }
        let status = GitStatusTool::new(repo.dir.path().to_path_buf(), env.clone(), TIMEOUT, CAP);
        let diff = GitDiffTool::new(repo.dir.path().to_path_buf(), env, TIMEOUT, CAP);
        status.call(&json!({})).unwrap();
        diff.call(&json!({})).unwrap();
        diff.call(&json!({"staged": true})).unwrap();
        marker.exists()
    }

    #[test]
    fn a_filter_with_an_odd_name_is_never_run() {
        for name in ODD_FILTER_NAMES {
            // Arrange
            let env = vec![(
                OsString::from("PATH"),
                std::env::var_os("PATH").unwrap_or_default(),
            )];

            // Act
            let ran = marker_after_calls_with_filter(name, env);

            // Assert
            assert!(!ran, "git ran a filter named {name:?}");
        }
    }

    #[test]
    fn a_missing_object_is_never_fetched_through_a_program_from_the_config() {
        // Arrange
        let repo = Repo::new();
        let marker = repo.dir.path().join("ran");
        let touch = format!("touch {}; false", marker.display());
        repo.write("a.txt", "one\n");
        repo.git(&["add", "."]);
        let blob = Command::new(GIT)
            .args(["rev-parse", ":a.txt"])
            .current_dir(repo.dir.path())
            .output()
            .unwrap()
            .stdout;
        let blob = String::from_utf8(blob).unwrap();
        let blob = blob.trim();
        let (folder, file) = blob.split_at(OBJECT_FOLDER_LEN);
        fs::remove_file(repo.dir.path().join(".git/objects").join(folder).join(file)).unwrap();
        repo.write("a.txt", "two\n");
        repo.git(&["config", "extensions.partialClone", "origin"]);
        repo.git(&["config", "remote.origin.promisor", "true"]);
        repo.git(&["config", "remote.origin.url", "ssh://example.invalid/x.git"]);
        repo.git(&["config", "core.sshCommand", &touch]);

        // Act
        repo.status().call(&json!({})).unwrap();
        repo.diff().call(&json!({})).unwrap();

        // Assert
        assert!(!marker.exists(), "git fetched through a configured program");
    }

    #[test]
    fn a_filter_set_in_the_callers_config_parameters_is_never_run() {
        // Arrange
        let repo = Repo::new();
        let marker = repo.dir.path().join("ran");
        let touch = format!("touch {}", marker.display());
        repo.write(".gitattributes", "* filter=evil\n");
        repo.write("a.txt", "one\n");
        repo.git(&["add", "."]);
        repo.write("a.txt", "two\n");
        let mut env = repo.env();
        env.push((
            "GIT_CONFIG_PARAMETERS".into(),
            format!("'filter.evil.clean={touch}'").into(),
        ));
        let diff = GitDiffTool::new(repo.dir.path().to_path_buf(), env, TIMEOUT, CAP);

        // Act
        diff.call(&json!({})).unwrap();

        // Assert
        assert!(!marker.exists(), "git ran a filter from the environment");
    }

    #[test]
    fn program_variables_in_the_callers_environment_are_never_run() {
        // Arrange
        let repo = Repo::new();
        let marker = repo.dir.path().join("ran");
        let touch = format!("touch {}", marker.display());
        repo.write(".gitattributes", "* diff=evil filter=evil\n");
        repo.write("a.txt", "one\n");
        repo.git(&["add", "."]);
        repo.write("a.txt", "two\n");
        let mut env = repo.env();
        env.extend([
            ("GIT_EXTERNAL_DIFF".into(), touch.clone().into()),
            ("GIT_PAGER".into(), touch.clone().into()),
            ("PAGER".into(), touch.clone().into()),
            ("GIT_SSH_COMMAND".into(), touch.clone().into()),
            ("GIT_CONFIG_COUNT".into(), "3".into()),
            ("GIT_CONFIG_KEY_0".into(), "core.fsmonitor".into()),
            ("GIT_CONFIG_VALUE_0".into(), touch.clone().into()),
            ("GIT_CONFIG_KEY_1".into(), "diff.evil.textconv".into()),
            ("GIT_CONFIG_VALUE_1".into(), touch.clone().into()),
            ("GIT_CONFIG_KEY_2".into(), "filter.evil.clean".into()),
            ("GIT_CONFIG_VALUE_2".into(), touch.clone().into()),
            (
                "GIT_CONFIG_PARAMETERS".into(),
                format!("'core.fsmonitor={touch}'").into(),
            ),
        ]);
        let status = GitStatusTool::new(repo.dir.path().to_path_buf(), env.clone(), TIMEOUT, CAP);
        let diff = GitDiffTool::new(repo.dir.path().to_path_buf(), env, TIMEOUT, CAP);

        // Act
        status.call(&json!({})).unwrap();
        diff.call(&json!({})).unwrap();

        // Assert
        assert!(!marker.exists(), "git ran a program from the environment");
    }

    #[test]
    fn config_that_includes_another_file_runs_nothing() {
        // Arrange
        let repo = Repo::new();
        let marker = repo.dir.path().join("ran");
        let touch = format!("touch {}", marker.display());
        repo.write("extra.cfg", &format!("[core]\n\tfsmonitor = {touch}\n[diff]\n\texternal = {touch}\n[pager]\n\tdiff = {touch}\n\tstatus = {touch}\n"));
        repo.write("a.txt", "one\n");
        repo.git(&["add", "."]);
        repo.write("a.txt", "two\n");
        repo.git(&["config", "include.path", "../extra.cfg"]);
        repo.git(&["config", "includeIf.gitdir:**.path", "../extra.cfg"]);
        let included = Command::new(GIT)
            .args(["config", "--get", "core.fsmonitor"])
            .current_dir(repo.dir.path())
            .output()
            .unwrap()
            .stdout;
        assert_eq!(String::from_utf8_lossy(&included).trim(), touch);

        // Act
        repo.status().call(&json!({})).unwrap();
        repo.diff().call(&json!({})).unwrap();

        // Assert
        assert!(!marker.exists(), "git ran a program from an included file");
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

    /// A stand-in `git` that reports `version` and records any other call.
    fn fake_git(version: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'git version {version}'; exit 0; fi\n: > {}\nexit 129\n",
            marker.display()
        );
        let path = dir.path().join(GIT);
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        (dir, marker)
    }

    fn tools_with_fake_git(dir: &Path) -> (GitStatusTool, GitDiffTool) {
        let env = vec![(OsString::from("PATH"), dir.as_os_str().to_owned())];
        (
            GitStatusTool::new(dir.to_path_buf(), env.clone(), TIMEOUT, CAP),
            GitDiffTool::new(dir.to_path_buf(), env, TIMEOUT, CAP),
        )
    }

    #[test]
    fn a_git_older_than_the_overrides_need_runs_nothing() {
        for version in [
            "2.30.2",
            "2.43.9 (Apple Git-1)",
            "1.8.3.1",
            "2.43.0.windows.1",
        ] {
            // Arrange
            let (dir, marker) = fake_git(version);
            let (status, diff) = tools_with_fake_git(dir.path());

            // Act
            let status_err = status.call(&json!({})).unwrap_err().to_string();
            let diff_err = diff.call(&json!({})).unwrap_err().to_string();

            // Assert
            assert!(status_err.contains("2.44"), "{version}: {status_err}");
            assert!(diff_err.contains("2.44"), "{version}: {diff_err}");
            assert!(
                !marker.exists(),
                "{version}: git ran after the version check"
            );
        }
    }

    #[test]
    fn a_git_that_does_not_report_a_version_runs_nothing() {
        // Arrange
        let (dir, marker) = fake_git("unparseable");
        let (status, _) = tools_with_fake_git(dir.path());

        // Act
        let err = status.call(&json!({})).unwrap_err().to_string();

        // Assert
        assert!(err.contains("version"), "{err}");
        assert!(!marker.exists());
    }

    #[test]
    fn a_git_at_the_minimum_version_is_allowed_to_run() {
        // Arrange
        let (dir, marker) = fake_git("2.44.0");
        let (status, _) = tools_with_fake_git(dir.path());

        // Act
        let _ = status.call(&json!({}));

        // Assert
        assert!(marker.exists(), "a supported git was refused");
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
