// SPDX-License-Identifier: AGPL-3.0-only

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use coda_config::config::Dirs;
use coda_config::timing::Timing;

use crate::error::CliError;
use crate::tui_cmd::{TuiLauncher, real_tui};

const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_WAIT_POLL: Duration = Duration::from_millis(50);

/// How long the CLI waits for a daemon to appear or disappear, and how often it checks.
#[derive(Debug, Clone, Copy)]
pub struct Wait {
    pub timeout: Duration,
    pub poll: Duration,
}

impl Default for Wait {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_WAIT_TIMEOUT,
            poll: DEFAULT_WAIT_POLL,
        }
    }
}

/// Everything a command reads from the outside world, injected so tests can drive it.
#[derive(Debug, Clone)]
pub struct Context {
    /// Where the config, data and socket live.
    pub dirs: Dirs,
    /// The binary started as `coda daemon` when a daemon is needed.
    pub exe: PathBuf,
    /// Directory commands treat as the current one.
    pub cwd: PathBuf,
    /// Request timeout toward the daemon.
    pub timing: Timing,
    /// Limits for waiting on daemon start and stop.
    pub wait: Wait,
    /// Whether standard input and output are both a terminal.
    pub is_tty: bool,
    /// Whether standard input is a terminal, whatever standard output is.
    pub stdin_is_tty: bool,
    /// Runs the terminal UI.
    pub launch_tui: TuiLauncher,
}

impl Context {
    /// A context with default timing and waits, no terminal and the real UI.
    #[must_use]
    pub fn new(dirs: Dirs, exe: PathBuf, cwd: PathBuf) -> Self {
        Self {
            dirs,
            exe,
            cwd,
            timing: Timing::default(),
            wait: Wait::default(),
            is_tty: false,
            stdin_is_tty: false,
            launch_tui: real_tui,
        }
    }

    /// The context of the running process: `CODA_HOME` through `coda-config`,
    /// the current executable and the current directory.
    ///
    /// # Errors
    /// Returns [`CliError::Config`] if neither `CODA_HOME` nor `HOME` is set, or
    /// [`CliError::Environment`] if the executable or directory cannot be found.
    pub fn from_env() -> Result<Self, CliError> {
        let mut ctx = Self::new(
            Dirs::from_env()?,
            std::env::current_exe().map_err(CliError::Environment)?,
            std::env::current_dir().map_err(CliError::Environment)?,
        );
        ctx.stdin_is_tty = std::io::stdin().is_terminal();
        ctx.is_tty = ctx.stdin_is_tty && std::io::stdout().is_terminal();
        Ok(ctx)
    }
}

/// Where a command writes its output and reads answers to prompts.
pub struct Io<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    pub input: &'a mut dyn BufRead,
}
