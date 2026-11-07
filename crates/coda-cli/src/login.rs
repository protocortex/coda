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

use clap::ValueEnum;
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
const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
const CODEX_HOME_ENV: &str = "CODEX_HOME";

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

    /// The config-dir variable that is set in this shell but that sessions do not get,
    /// so a login written there would not be seen by them.
    fn hidden_env(
        self,
        passthrough: &[String],
        is_set: &dyn Fn(&str) -> bool,
    ) -> Option<&'static str> {
        let (name, passed_on) = match self {
            Self::ClaudeCode => (
                CLAUDE_CONFIG_DIR_ENV,
                passthrough
                    .iter()
                    .any(|listed| listed == CLAUDE_CONFIG_DIR_ENV),
            ),
            Self::Codex => (CODEX_HOME_ENV, false),
        };
        (is_set(name) && !passed_on).then_some(name)
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
                resume_thread: None,
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
    log_in(io, backend, binary, &config, args.yes).await
}

/// Offers a login to each enabled backend that the probe finds logged out. A login
/// that fails is reported and does not stop the caller, since the UI can still open.
///
/// # Errors
/// Returns an error only when the output cannot be written.
pub(crate) async fn offer_logins(io: &mut Io<'_>, config: &Config) -> Result<(), CliError> {
    let for_locate = config.clone();
    let located = tokio::task::spawn_blocking(move || {
        LoginBackend::value_variants()
            .iter()
            .filter_map(|backend| {
                locate_binary(*backend, &for_locate)
                    .ok()
                    .map(|binary| (*backend, binary))
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let passthrough = claude_env_passthrough(config);
    let is_set = |name: &str| std::env::var_os(name).is_some();
    for (backend, binary) in located {
        let probed = probe_account(backend, binary.clone(), config).await;
        let key = backend.key();
        match offer_for(backend, probed, &passthrough, &is_set) {
            Offer::None => {}
            Offer::Unchecked(reason) => {
                writeln!(io.err, "{key} login not checked: {reason}")?;
            }
            Offer::Hidden(name) => writeln!(
                io.err,
                "{key} looks logged out to coda, but {name} is set here and sessions do not get it; the login may be stored there, so no login is offered"
            )?,
            Offer::Ask => {
                writeln!(io.out, "{key} is not logged in")?;
                if let Err(error) = run_login(io, backend, binary, config, false).await {
                    writeln!(io.err, "{error}")?;
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Offer {
    None,
    Ask,
    Hidden(&'static str),
    Unchecked(String),
}

/// What to do about a probe result. A hidden config dir can hold a login the
/// probe cannot see, so it is never answered with a login the user may not need.
fn offer_for(
    backend: LoginBackend,
    probed: Result<AccountStatus, String>,
    passthrough: &[String],
    is_set: &dyn Fn(&str) -> bool,
) -> Offer {
    match probed {
        Ok(AccountStatus::LoggedIn(_)) => Offer::None,
        Err(reason) => Offer::Unchecked(reason),
        Ok(AccountStatus::LoggedOut) => backend
            .hidden_env(passthrough, is_set)
            .map_or(Offer::Ask, Offer::Hidden),
    }
}

async fn log_in(
    io: &mut Io<'_>,
    backend: LoginBackend,
    binary: PathBuf,
    config: &Config,
    yes: bool,
) -> Result<ExitCode, CliError> {
    let key = backend.key();
    if let Ok(AccountStatus::LoggedIn(account)) =
        probe_account(backend, binary.clone(), config).await
    {
        writeln!(io.out, "{key} is already logged in{}", who(&account))?;
        if account.billing == AccountBilling::ApiKey {
            writeln!(io.out, "{API_KEY_NOTE}")?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    run_login(io, backend, binary, config, yes).await
}

/// Asks, runs the vendor login with this terminal, and checks the result.
async fn run_login(
    io: &mut Io<'_>,
    backend: LoginBackend,
    binary: PathBuf,
    config: &Config,
    yes: bool,
) -> Result<ExitCode, CliError> {
    let key = backend.key();
    let command = backend.login_text();
    let is_set = |name: &str| std::env::var_os(name).is_some();
    if let Some(name) = backend.hidden_env(&claude_env_passthrough(config), &is_set) {
        writeln!(
            io.out,
            "{name} is set here, but coda sessions do not get it, so they will not see a login stored there"
        )?;
    }
    if !yes && !confirm(io, &command)? {
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
    match probe_account(backend, binary, config).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |name| names.contains(&name)
    }

    #[test]
    fn a_logged_out_probe_asks_unless_a_hidden_config_dir_may_hold_the_login() {
        // Arrange
        let out = || Ok(AccountStatus::LoggedOut);

        // Act
        let asks = offer_for(LoginBackend::Codex, out(), &[], &set(&[]));
        let hidden = offer_for(LoginBackend::Codex, out(), &[], &set(&["CODEX_HOME"]));

        // Assert
        assert_eq!(asks, Offer::Ask);
        assert_eq!(hidden, Offer::Hidden("CODEX_HOME"));
    }

    #[test]
    fn a_probe_that_failed_is_reported_and_a_login_is_left_alone() {
        // Arrange
        let failed = Err("timed out".to_owned());

        // Act
        let offer = offer_for(LoginBackend::Codex, failed, &[], &set(&[]));

        // Assert
        assert_eq!(offer, Offer::Unchecked("timed out".to_owned()));
    }

    #[test]
    fn a_logged_in_probe_gets_no_offer() {
        // Arrange
        let account = Account {
            plan: None,
            email: None,
            billing: AccountBilling::Subscription,
            api_key_source: None,
        };

        // Act
        let offer = offer_for(
            LoginBackend::Codex,
            Ok(AccountStatus::LoggedIn(account)),
            &[],
            &set(&[]),
        );

        // Assert
        assert_eq!(offer, Offer::None);
    }

    #[test]
    fn a_config_dir_that_sessions_do_not_get_is_named_before_the_login() {
        // Arrange
        let is_set = set(&["CLAUDE_CONFIG_DIR", "CODEX_HOME"]);

        // Act
        let claude = LoginBackend::ClaudeCode.hidden_env(&[], &is_set);
        let codex = LoginBackend::Codex.hidden_env(&[], &is_set);

        // Assert
        assert_eq!(claude, Some("CLAUDE_CONFIG_DIR"));
        assert_eq!(codex, Some("CODEX_HOME"));
    }

    #[test]
    fn a_config_dir_the_config_passes_on_or_that_is_unset_is_not_named() {
        // Arrange
        let passed = vec!["CLAUDE_CONFIG_DIR".to_owned()];

        // Act
        let passed_on = LoginBackend::ClaudeCode.hidden_env(&passed, &set(&["CLAUDE_CONFIG_DIR"]));
        let unset = LoginBackend::Codex.hidden_env(&[], &set(&[]));

        // Assert
        assert_eq!(passed_on, None);
        assert_eq!(unset, None);
    }
}
