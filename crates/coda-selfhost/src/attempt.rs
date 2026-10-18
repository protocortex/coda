// SPDX-License-Identifier: AGPL-3.0-only

//! One `coda run` attempt at the pinned task.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::error::SelfhostError;
use crate::process::run_bounded;
use crate::task::{GRANTS, MAX_TURNS, PROMPT, TOKEN_CAP};

const CONFIG_FILE: &str = "config.toml";
const STATUS_LIMIT: Duration = Duration::from_secs(10);
const PID_PREFIX: &str = "pid: ";
const STOP_LIMIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone)]
pub struct AttemptSpec<'a> {
    pub coda: &'a Path,
    pub home: &'a Path,
    pub workdir: &'a Path,
    /// The faux provider script `coda run` replays.
    pub script: PathBuf,
    pub limit: Duration,
}

/// What `coda run` printed and how it exited.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The `config.toml` that replays `script` and pre-grants the pinned task's commands.
#[must_use]
pub fn config_toml(script: &Path) -> String {
    let grants = GRANTS.map(|grant| format!("{grant:?}")).join(", ");
    format!(
        "[permissions]\ngrants = [{grants}]\n\n[providers.faux]\nenabled = true\nscript = {:?}\n",
        script.display().to_string()
    )
}

/// Runs the pinned task with `coda run` in `spec.workdir`; the daemon it
/// starts is stopped before this returns.
pub fn run_attempt(spec: &AttemptSpec<'_>) -> Result<Transcript, SelfhostError> {
    let config = spec.home.join(CONFIG_FILE);
    fs::write(&config, config_toml(&spec.script)).map_err(|source| SelfhostError::Io {
        path: config,
        source,
    })?;
    let _daemon = DaemonGuard(spec);
    let done = run_bounded(
        Command::new(spec.coda)
            .args(["run", "--prompt", PROMPT, "--mode", "accept-edits"])
            .args(["--max-turns", &MAX_TURNS.to_string()])
            .args(["--token-cap", &TOKEN_CAP.to_string()])
            .arg("--cwd")
            .arg(spec.workdir)
            .env("CODA_HOME", spec.home)
            .current_dir(spec.workdir),
        spec.limit,
    )?;
    Ok(Transcript {
        exit_code: done.status.code(),
        stdout: done.stdout,
        stderr: done.stderr,
    })
}

/// Stops the daemon `coda run` started, even when the run fails or times out.
struct DaemonGuard<'a, 'b>(&'a AttemptSpec<'b>);

impl Drop for DaemonGuard<'_, '_> {
    fn drop(&mut self) {
        let status = run_bounded(
            Command::new(self.0.coda)
                .arg("status")
                .env("CODA_HOME", self.0.home),
            STATUS_LIMIT,
        );
        let Ok(status) = status else { return };
        let pid = status
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix(PID_PREFIX));
        if let Some(pid) = pid {
            let _ = run_bounded(Command::new("kill").arg(pid), STATUS_LIMIT);
            wait_gone(pid);
        }
    }
}

fn is_alive(pid: &str) -> bool {
    run_bounded(Command::new("kill").args(["-0", pid]), STATUS_LIMIT)
        .is_ok_and(|done| done.status.success())
}

/// Waits, up to `STOP_LIMIT`, for `pid` to exit, so the directory it uses can
/// be removed safely.
fn wait_gone(pid: &str) {
    let deadline = Instant::now() + STOP_LIMIT;
    while is_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_returns_only_once_the_process_has_exited() {
        // Arrange
        let mut child = Command::new("sleep").arg("0.4").spawn().unwrap();
        let pid = child.id().to_string();
        std::thread::spawn(move || child.wait());

        // Act
        wait_gone(&pid);

        // Assert
        assert!(!is_alive(&pid));
    }

    #[test]
    fn the_config_replays_the_script_and_grants_the_pinned_commands() {
        // Act
        let config = config_toml(Path::new("/h/script.json"));

        // Assert
        assert_eq!(
            config,
            "[permissions]\n\
             grants = [\"bash:cargo test\", \"bash:cargo build\", \"edit:crates/coda-cli/src/main.rs\"]\n\n\
             [providers.faux]\nenabled = true\nscript = \"/h/script.json\"\n"
        );
    }
}
