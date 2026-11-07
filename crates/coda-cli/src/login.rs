// SPDX-License-Identifier: Apache-2.0

//! `coda login`: hands the login to the vendor CLI in the user's terminal.
//!
//! Coda only asks the backend binary whether it is logged in, runs the binary's
//! own login command with the terminal's streams, and asks again. The browser flow
//! and the token stay between the user and the vendor CLI; coda never sees either,
//! and it has no login of its own.
//!
//! The commands were checked against claude 2.1.296 (`claude auth login`) and
//! codex-cli 0.163.0 (`codex login`) by reading their `--help`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use coda_config::config::Config;
use coda_daemon::backends::{ClaudeCodeBackend, CodexBackend, claude_env_passthrough};
use coda_protocol::events::AccountBilling;
use coda_protocol::types::SessionId;
use coda_providers::account::{Account, AccountStatus, probe_claude};
use coda_providers::backend::{AgentBackend, locate};
use coda_providers::codex_app_server::{AppServerSpec, read_account};
use coda_providers::codex_exec::SandboxMode;

use crate::cli::{LoginArgs, LoginBackend};
use crate::context::{Context, Io};
use crate::daemon_cmd::load_config;
use crate::error::CliError;
use crate::exit;

/// How long a login probe may take; it starts the backend's own binary.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const YES_ANSWERS: [&str; 2] = ["y", "yes"];
const PROBE_SESSION: &str = "login";
const API_KEY_NOTE: &str = "it bills API rates through an API key; coda login does not switch accounts, so log out with the vendor CLI first";
const SESSION_ENV_NOTE: &str = "CLAUDE_CONFIG_DIR and CODEX_HOME";

impl LoginBackend {
    pub(crate) fn backend(self) -> &'static dyn AgentBackend {
        match self {
            Self::ClaudeCode => &ClaudeCodeBackend,
            Self::Codex => &CodexBackend,
        }
    }

    pub(crate) fn key(self) -> &'static str {
        self.backend().config_key()
    }

    fn login_args(self) -> &'static [&'static str] {
        match self {
            Self::ClaudeCode => &["auth", "login"],
            Self::Codex => &["login"],
        }
    }

    /// The vendor command as the user would type it.
    pub(crate) fn login_text(self) -> String {
        let program = self.backend().binary_name();
        format!("{program} {}", self.login_args().join(" "))
    }

    /// The flag form of the name, as `coda login` takes it.
    pub(crate) fn flag_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }
}

/// Finds the enabled backend's binary. Runs `--version`, so call it off the runtime.
pub(crate) fn locate_binary(backend: LoginBackend, config: &Config) -> Result<PathBuf, String> {
    let search_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    locate(backend.backend(), config, &search_dirs)
        .map(|found| found.path)
        .map_err(|error| error.to_string())
}

/// Asks the binary which login it runs on, with the environment a session gets.
pub(crate) async fn probe_account(
    backend: LoginBackend,
    binary: PathBuf,
    config: &Config,
) -> Result<AccountStatus, String> {
    let env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    match backend {
        LoginBackend::ClaudeCode => {
            let passthrough = claude_env_passthrough(config);
            tokio::task::spawn_blocking(move || {
                probe_claude(&binary, &env, &passthrough, PROBE_TIMEOUT)
            })
            .await
            .unwrap_or_else(|error| Err(format!("the probe did not finish: {error}")))
        }
        LoginBackend::Codex => {
            let spec = AppServerSpec {
                binary,
                cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                session_id: SessionId::new(PROBE_SESSION),
                sandbox: SandboxMode::default(),
                ephemeral: true,
                verified_versions: Vec::new(),
                parent_env: env,
            };
            read_account(&spec).await.map_err(|error| error.to_string())
        }
    }
}

/// Logs a backend in through its own CLI.
///
/// # Errors
/// Returns [`CliError::Login`] when the backend is not enabled or found, or cannot be started.
pub async fn run(ctx: &Context, io: &mut Io<'_>, args: LoginArgs) -> Result<ExitCode, CliError> {
    let backend = args.backend;
    let config = load_config(&ctx.dirs)?;
    let for_locate = config.clone();
    let located = tokio::task::spawn_blocking(move || locate_binary(backend, &for_locate))
        .await
        .map_err(|error| CliError::Login(error.to_string()))?;
    let binary = located.map_err(CliError::Login)?;
    let key = backend.key();
    if let Ok(AccountStatus::LoggedIn(account)) =
        probe_account(backend, binary.clone(), &config).await
    {
        writeln!(io.out, "{key} is already logged in{}", who(&account))?;
        if account.billing == AccountBilling::ApiKey {
            writeln!(io.out, "{API_KEY_NOTE}")?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    let command = backend.login_text();
    if !args.yes && !confirm(io, &command)? {
        writeln!(io.out, "{key} was left logged out")?;
        return Ok(ExitCode::from(exit::FAILURE));
    }
    writeln!(
        io.out,
        "running {} in this terminal; `{command}` talks to you here",
        binary.display()
    )?;
    io.out.flush()?;
    let status = hand_over(binary.clone(), backend.login_args()).await?;
    if !status.success() {
        return Err(CliError::Login(format!("`{command}` exited with {status}")));
    }
    match probe_account(backend, binary, &config).await {
        Ok(AccountStatus::LoggedIn(account)) => {
            writeln!(io.out, "{key} is logged in{}", who(&account))?;
            Ok(ExitCode::SUCCESS)
        }
        Ok(AccountStatus::LoggedOut) => Err(CliError::Login(format!(
            "{key} is still not logged in after `{command}`; coda checks with the environment a session gets, which leaves out {SESSION_ENV_NOTE}"
        ))),
        Err(reason) => Err(CliError::Login(format!(
            "`{command}` finished but the login could not be checked: {reason}"
        ))),
    }
}

fn who(account: &Account) -> String {
    account
        .plan
        .as_ref()
        .map_or(String::new(), |plan| format!(" ({plan})"))
}

fn confirm(io: &mut Io<'_>, command: &str) -> Result<bool, CliError> {
    write!(io.out, "Run `{command}` in this terminal? [y/N] ")?;
    io.out.flush()?;
    let mut answer = String::new();
    io.input.read_line(&mut answer)?;
    Ok(YES_ANSWERS.contains(&answer.trim().to_lowercase().as_str()))
}

/// Runs the vendor command with this terminal's streams and the user's own
/// environment, so its browser flow works. Coda reads nothing it prints or stores.
/// A browser login can take minutes, so the wait runs off the runtime.
async fn hand_over(
    binary: PathBuf,
    args: &'static [&'static str],
) -> Result<std::process::ExitStatus, CliError> {
    let exe = binary.clone();
    let ran = tokio::task::spawn_blocking(move || {
        Command::new(&binary)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
    })
    .await
    .map_err(|error| CliError::Login(error.to_string()))?;
    ran.map_err(|source| CliError::Spawn { exe, source })
}
