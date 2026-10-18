// SPDX-License-Identifier: AGPL-3.0-only

//! One `coda run` attempt at the pinned task.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::error::SelfhostError;
use crate::process::{run_bounded, run_cleanup};
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
    pub provider: Provider,
    pub limit: Duration,
}

/// The only Anthropic model a live run asks for.
pub const MODEL: &str = "claude-sonnet-5-5";

/// What serves the model calls of `coda run`.
#[derive(Debug, Clone)]
pub enum Provider {
    /// Replays a faux provider script file.
    Faux { script: PathBuf },
    /// The Anthropic API; the key is read from the named environment variable.
    Anthropic { api_key_env: String },
}

impl Provider {
    /// The environment variable that holds the provider's API key, if any.
    #[must_use]
    pub fn key_env(&self) -> Option<&str> {
        match self {
            Self::Faux { .. } => None,
            Self::Anthropic { api_key_env } => Some(api_key_env),
        }
    }
}

/// What `coda run` printed and how it exited.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The `config.toml` that selects `provider` and pre-grants the pinned task's
/// commands. It names the key variable and never holds a key.
#[must_use]
pub fn config_toml(provider: &Provider) -> String {
    let grants = GRANTS.map(|grant| format!("{grant:?}")).join(", ");
    let section = match provider {
        Provider::Faux { script } => format!(
            "[providers.faux]\nenabled = true\nscript = {:?}\n",
            script.display().to_string()
        ),
        Provider::Anthropic { api_key_env } => format!(
            "[providers.anthropic]\nenabled = true\napi_key_env = {api_key_env:?}\nmodel = {MODEL:?}\n"
        ),
    };
    format!("[permissions]\ngrants = [{grants}]\n\n{section}")
}

/// Runs the pinned task with `coda run` in `spec.workdir`; the daemon it
/// starts is stopped before this returns.
pub fn run_attempt(spec: &AttemptSpec<'_>) -> Result<Transcript, SelfhostError> {
    let config = spec.home.join(CONFIG_FILE);
    fs::write(&config, config_toml(&spec.provider)).map_err(|source| SelfhostError::Io {
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
        let mut status_command = Command::new(self.0.coda);
        status_command.arg("status").env("CODA_HOME", self.0.home);
        if let Some(name) = self.0.provider.key_env() {
            status_command.env_remove(name);
        }
        let status = run_cleanup(&mut status_command, STATUS_LIMIT);
        let Ok(status) = status else { return };
        let pid = status
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix(PID_PREFIX));
        if let Some(pid) = pid {
            let _ = run_cleanup(Command::new("kill").arg(pid), STATUS_LIMIT);
            wait_gone(pid);
        }
    }
}

fn is_alive(pid: &str) -> bool {
    run_cleanup(Command::new("kill").args(["-0", pid]), STATUS_LIMIT)
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
        let config = config_toml(&Provider::Faux {
            script: PathBuf::from("/h/script.json"),
        });

        // Assert
        assert_eq!(
            config,
            "[permissions]\n\
             grants = [\"bash:cargo test\", \"bash:cargo build\", \"edit:crates/coda-cli/src/main.rs\"]\n\n\
             [providers.faux]\nenabled = true\nscript = \"/h/script.json\"\n"
        );
    }

    #[test]
    fn the_anthropic_config_names_the_key_variable_and_the_pinned_model() {
        // Act
        let config = config_toml(&Provider::Anthropic {
            api_key_env: "ANTHROPIC_API_KEY".to_owned(),
        });

        // Assert
        assert_eq!(
            config,
            "[permissions]\n\
             grants = [\"bash:cargo test\", \"bash:cargo build\", \"edit:crates/coda-cli/src/main.rs\"]\n\n\
             [providers.anthropic]\nenabled = true\napi_key_env = \"ANTHROPIC_API_KEY\"\n\
             model = \"claude-sonnet-5-5\"\n"
        );
    }
}
