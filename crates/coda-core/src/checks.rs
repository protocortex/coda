// SPDX-License-Identifier: Apache-2.0

//! Checks: a command the user chose for a file extension, run after the model
//! changes a file of that kind, so the model sees what it broke at once.
//!
//! A check runs the way a hook does (no shell, the hook environment, its process
//! group killed at the timeout). It never fails the change it follows: the
//! outcome is a few lines added to the tool's result.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use coda_config::checks::CheckConfig;

use crate::economy::{cap_ends, compress::compress_log};
use crate::hooks::{CommandEnd, HookRunner, plain_lines};

/// The tools whose success changes files.
const CHANGING_TOOLS: [&str; 3] = ["edit", "write", "multi_edit"];
/// The most of a check's output read, counted from its end.
const KEEP_OUTPUT_BYTES: usize = 32 * 1024;
const PASSED: &str = "check passed";
const FAILED: &str = "check failed";
const TIMED_OUT: &str = "check timed out";
const NOT_RUN: &str = "check could not run";
const NO_PROGRAM: &str = "(no program)";

/// The checks of a session, by file extension.
#[derive(Debug, Clone, Default)]
pub struct Checks {
    by_extension: BTreeMap<String, CheckConfig>,
    runner: HookRunner,
}

impl Checks {
    /// Checks for `config`, run with the environment `env` and nothing else.
    #[must_use]
    pub fn new(config: &BTreeMap<String, CheckConfig>, env: Vec<(OsString, OsString)>) -> Self {
        let by_extension = config
            .iter()
            .map(|(extension, check)| (extension.to_ascii_lowercase(), check.clone()))
            .collect();
        Self {
            by_extension,
            runner: HookRunner::new(&[], env),
        }
    }

    /// Whether any check is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_extension.is_empty()
    }

    /// Whether a call to `tool` changes files, so a check may follow it.
    #[must_use]
    pub fn follows(tool: &str) -> bool {
        CHANGING_TOOLS.contains(&tool)
    }

    /// Runs the checks for the files in `paths`, each distinct command once, in
    /// `cwd`. `None` when no path has a check, so nothing is to be said.
    pub async fn run(&self, cwd: &Path, paths: &[PathBuf]) -> Option<String> {
        let mut commands: Vec<&CheckConfig> = Vec::new();
        for path in paths {
            let Some(check) = self.check_for(path) else {
                continue;
            };
            if !commands.iter().any(|seen| seen.command == check.command) {
                commands.push(check);
            }
        }
        if commands.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        for check in commands {
            lines.push(self.run_one(check, cwd).await);
        }
        Some(lines.join("\n"))
    }

    fn check_for(&self, path: &Path) -> Option<&CheckConfig> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        self.by_extension.get(&extension)
    }

    async fn run_one(&self, check: &CheckConfig, cwd: &Path) -> String {
        let program = check.command.first().map_or(NO_PROGRAM, String::as_str);
        let run = self
            .runner
            .run_command(&check.command, cwd, check.timeout(), KEEP_OUTPUT_BYTES)
            .await;
        match run.end {
            CommandEnd::Passed => format!("{PASSED} ({program})"),
            CommandEnd::Failed(why) => {
                let head = format!("{FAILED} ({program}, {why})");
                let output = short_output(&run.output);
                if output.is_empty() {
                    head
                } else {
                    format!("{head}:\n{output}")
                }
            }
            CommandEnd::TimedOut(limit) => format!(
                "{TIMED_OUT} after {} ms ({program}); the change was applied",
                limit.as_millis()
            ),
            CommandEnd::NotRun(why) => format!("{NOT_RUN} ({program}): {why}"),
        }
    }
}

/// A check's output as the model should see it: plain text, with a long log
/// cut to its errors, its start and its end.
fn short_output(output: &str) -> String {
    let plain = plain_lines(output);
    cap_ends(compress_log(plain.as_bytes()).trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(20);
    /// Long enough for a loaded machine to start the script before it is cut.
    const HOLD_TIMEOUT_MS: u64 = 2_000;

    fn checks(entries: &[(&str, &str)]) -> Checks {
        let config = entries
            .iter()
            .map(|(extension, script)| {
                let check = CheckConfig {
                    command: vec!["/bin/sh".into(), "-c".into(), (*script).into()],
                    timeout_ms: Some(15_000),
                };
                ((*extension).to_owned(), check)
            })
            .collect();
        Checks::new(&config, Vec::new())
    }

    async fn run(checks: &Checks, paths: &[&str]) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        tokio::time::timeout(WAIT, checks.run(dir.path(), &paths))
            .await
            .expect("the check did not finish in time")
    }

    #[tokio::test]
    async fn a_path_without_a_configured_extension_says_nothing() {
        // Arrange
        let checks = checks(&[("rs", "exit 1")]);

        // Act
        let said = run(&checks, &["notes.md", "Makefile", "dir/.hidden"]).await;

        // Assert
        assert_eq!(said, None);
    }

    #[tokio::test]
    async fn an_extension_matches_without_regard_to_case() {
        // Arrange
        let checks = checks(&[("RS", "exit 0")]);

        // Act
        let said = run(&checks, &["src/LIB.Rs"]).await;

        // Assert
        assert!(said.unwrap().starts_with(PASSED));
    }

    #[tokio::test]
    async fn one_command_for_two_extensions_runs_once() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("runs");
        let script = format!("echo run >> {}", log.display());
        let checks = checks(&[("ts", &script), ("tsx", &script)]);
        let paths = [PathBuf::from("a.ts"), PathBuf::from("b.tsx")];

        // Act
        let said = tokio::time::timeout(WAIT, checks.run(dir.path(), &paths))
            .await
            .unwrap();

        // Assert
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
        assert_eq!(said.unwrap().lines().count(), 1);
    }

    #[tokio::test]
    async fn two_commands_each_get_a_line() {
        // Arrange
        let checks = checks(&[("rs", "exit 0"), ("py", "exit 2")]);

        // Act
        let said = run(&checks, &["a.rs", "b.py"]).await.unwrap();

        // Assert
        let lines: Vec<&str> = said.lines().collect();
        assert_eq!(lines.len(), 2, "{said}");
        assert!(lines[0].starts_with(PASSED), "{said}");
        assert!(lines[1].starts_with(FAILED), "{said}");
    }

    #[tokio::test]
    async fn a_long_noisy_failure_is_made_plain_and_short() {
        // Arrange
        let script = "i=0; while [ $i -lt 3000 ]; do echo \"note: line $i\"; i=$((i+1)); done; \
                      printf 'error: the real problem \\033[31mred\\033[0m\\n' >&2; exit 1";
        let checks = checks(&[("rs", script)]);

        // Act
        let said = run(&checks, &["a.rs"]).await.unwrap();

        // Assert
        assert!(said.contains("the real problem"), "{said}");
        assert!(!said.contains('\u{1b}'), "{said:?}");
        assert!(said.len() < 9 * 1024, "{} bytes", said.len());
    }

    #[tokio::test]
    async fn colour_codes_and_line_separators_are_removed() {
        // Arrange
        let script = "printf 'error: \\033[1;31mred\\033[0m\\342\\200\\250split\\n' >&2; exit 1";
        let checks = checks(&[("rs", script)]);

        // Act
        let said = run(&checks, &["a.rs"]).await.unwrap();

        // Assert
        assert!(said.contains("error: redsplit"), "{said:?}");
    }

    #[tokio::test]
    async fn json_output_is_not_rewritten_as_a_tool_result() {
        // Arrange
        let checks = checks(&[("js", "echo '[{\"rule\":\"no-undef\"}]'; exit 1")]);

        // Act
        let said = run(&checks, &["a.js"]).await.unwrap();

        // Assert
        assert!(said.contains("[{\"rule\":\"no-undef\"}]"), "{said}");
    }

    #[tokio::test]
    async fn a_timed_out_check_takes_its_children_with_it() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let hold = dir.path().join("hold");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&hold)
                .status()
                .unwrap()
                .success()
        );
        // The script and the child it leaves behind each keep the write end
        // open, so the read end reaches its end only once both are gone.
        let started = dir.path().join("started");
        let script = format!(
            "exec 3>{}; : > {}; sleep 30 & wait",
            hold.display(),
            started.display()
        );
        let config = BTreeMap::from([(
            "rs".to_owned(),
            CheckConfig {
                command: vec!["/bin/sh".into(), "-c".into(), script],
                timeout_ms: Some(HOLD_TIMEOUT_MS),
            },
        )]);
        let checks = Checks::new(&config, Vec::new());
        let reader_path = hold.clone();
        let reader = tokio::task::spawn_blocking(move || {
            let mut file = std::fs::File::open(&reader_path).unwrap();
            std::io::Read::read_to_end(&mut file, &mut Vec::new()).unwrap();
        });

        // Act
        let said = tokio::time::timeout(WAIT, checks.run(dir.path(), &[PathBuf::from("a.rs")]))
            .await
            .unwrap()
            .unwrap();

        // Assert
        assert!(said.starts_with(TIMED_OUT), "{said}");
        if !started.exists() {
            // Lets the reader, still waiting for a writer, end before the failure.
            drop(std::fs::OpenOptions::new().write(true).open(&hold));
            reader.await.unwrap();
            panic!("the script was cut before it started; raise HOLD_TIMEOUT_MS");
        }
        tokio::time::timeout(WAIT, reader)
            .await
            .expect("a child outlived the check")
            .unwrap();
    }

    #[tokio::test]
    async fn a_program_that_cannot_start_is_reported() {
        // Arrange
        let config = BTreeMap::from([(
            "rs".to_owned(),
            CheckConfig {
                command: vec!["no-such-checker".into()],
                timeout_ms: None,
            },
        )]);
        let checks = Checks::new(&config, Vec::new());

        // Act
        let said = run(&checks, &["a.rs"]).await.unwrap();

        // Assert
        assert!(said.starts_with(NOT_RUN), "{said}");
    }

    #[test]
    fn only_file_changing_tools_are_followed_by_a_check() {
        // Assert
        for tool in ["edit", "write", "multi_edit"] {
            assert!(Checks::follows(tool), "{tool}");
        }
        for tool in ["read", "bash", "grep", "list", "task"] {
            assert!(!Checks::follows(tool), "{tool}");
        }
    }
}
