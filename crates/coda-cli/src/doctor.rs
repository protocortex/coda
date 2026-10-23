// SPDX-License-Identifier: AGPL-3.0-only

//! `coda doctor`: reports the health of the daemon's files, the database, the
//! backends and the config, and with `--repair` clears what a dead daemon left.

use std::fs::{self, File, TryLockError};
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use coda_config::config::{Config, ConfigError};
use coda_daemon::backends::{ClaudeCodeBackend, CodexBackend};
use coda_daemon::reaper::survey_orphans;
use coda_daemon::repair::{RepairError, RepairReport, repair};
use coda_protocol::methods::DaemonStatusResult;
use coda_protocol::types::BackendKind;
use coda_providers::backend::{AgentBackend, locate, require_enabled};
use coda_providers::registry::{ANTHROPIC_KEY, FAUX_KEY};
use coda_store::health::DatabaseRelease;
use coda_store::{BINARY_RELEASE, SCHEMA_VERSION, health};
use serde::Serialize;
use tracing::{info, warn};

use crate::autostart::{connect_unchecked, status_of};
use crate::cli::DoctorArgs;
use crate::context::{Context, Io};
use crate::error::CliError;
use crate::exit;
use crate::run::pick_backend;

const NAME_WIDTH: usize = 9;
const STATUS_WIDTH: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    status: Status,
    detail: String,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Ok, detail)
    }

    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Warn, detail)
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Fail, detail)
    }

    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Serialize)]
struct Report {
    healthy: bool,
    checks: Vec<Check>,
    repairs: Vec<String>,
}

impl Report {
    fn new(checks: Vec<Check>, repairs: Vec<String>) -> Self {
        let healthy = checks.iter().all(|c| c.status == Status::Ok);
        Self {
            healthy,
            checks,
            repairs,
        }
    }

    fn exit_code(&self) -> ExitCode {
        let worst = self.checks.iter().map(|c| c.status).max();
        match worst {
            Some(Status::Fail) => ExitCode::from(exit::DOCTOR_FAILURES),
            Some(Status::Warn) => ExitCode::from(exit::DOCTOR_WARNINGS),
            _ => ExitCode::SUCCESS,
        }
    }

    fn write_table(&self, out: &mut dyn io::Write) -> io::Result<()> {
        for check in &self.checks {
            writeln!(
                out,
                "{:<NAME_WIDTH$} {:<STATUS_WIDTH$} {}",
                check.name,
                check.status.label(),
                check.detail
            )?;
        }
        for repair in &self.repairs {
            writeln!(out, "repaired: {repair}")?;
        }
        Ok(())
    }
}

/// Runs the checks, repairing first when asked, and prints the report. The
/// exit code says whether anything is still wrong afterwards.
///
/// # Errors
/// Returns [`CliError::Output`] when the report cannot be written or
/// [`CliError::Json`] when it cannot be encoded.
pub async fn run(ctx: &Context, io: &mut Io<'_>, args: DoctorArgs) -> Result<ExitCode, CliError> {
    info!(step = "probe_daemon", "asking whether a daemon answers");
    let daemon = probe_daemon(ctx).await;
    let repairs = if args.repair {
        info!(step = "repair", "repairing leftovers");
        repair_leftovers(ctx, io, daemon.is_some()).await?
    } else {
        Vec::new()
    };
    info!(step = "check", "running the checks");
    let report = Report::new(run_checks(ctx, daemon.as_ref()).await, repairs);
    if args.json {
        writeln!(io.out, "{}", serde_json::to_string(&report)?)?;
    } else {
        report.write_table(io.out)?;
    }
    Ok(report.exit_code())
}

async fn probe_daemon(ctx: &Context) -> Option<DaemonStatusResult> {
    let client = connect_unchecked(ctx).await.ok()?;
    status_of(&client).await.ok()
}

async fn repair_leftovers(
    ctx: &Context,
    io: &mut Io<'_>,
    daemon_answers: bool,
) -> Result<Vec<String>, CliError> {
    if daemon_answers {
        writeln!(io.err, "coda: a daemon is running, nothing was repaired")?;
        return Ok(Vec::new());
    }
    match repair(&ctx.dirs.socket_path(), &ctx.dirs.run_dir()).await {
        Ok(done) => {
            if done.socket_path_blocked {
                writeln!(
                    io.err,
                    "coda: {} is not a socket, so it was left alone; remove it by hand",
                    ctx.dirs.socket_path().display()
                )?;
            }
            Ok(describe_repairs(&done))
        }
        // A held lock or an unexpected file is a finding, not a failure of the command.
        Err(e @ (RepairError::DaemonRunning { .. } | RepairError::Socket(_))) => {
            warn!(error = %e, "repair stopped");
            writeln!(io.err, "coda: {e}")?;
            Ok(Vec::new())
        }
        Err(e) => Err(e.into()),
    }
}

fn describe_repairs(done: &RepairReport) -> Vec<String> {
    let mut repairs = Vec::new();
    if done.socket_removed {
        repairs.push("removed a stale socket".to_owned());
    }
    if done.reaped.killed > 0 {
        repairs.push(format!(
            "killed {} orphaned backend process group(s)",
            done.reaped.killed
        ));
    }
    if done.reaped.stale > 0 {
        repairs.push(format!("removed {} stale pidfile(s)", done.reaped.stale));
    }
    repairs
}

async fn run_checks(ctx: &Context, daemon: Option<&DaemonStatusResult>) -> Vec<Check> {
    let (config_check, config) = check_config(&ctx.dirs.config_file());
    vec![
        check_socket(ctx, daemon),
        check_lock(ctx, daemon),
        check_orphans(ctx, daemon).await,
        check_database(ctx, daemon),
        check_schema(ctx, daemon),
        check_release(ctx, daemon),
        config_check,
        check_backends(config).await,
    ]
}

enum LockState {
    Absent,
    Free,
    Held,
}

/// Probes the lock with a read-only open, so it neither creates the file nor
/// changes its mode the way taking the lock as a daemon does.
fn lock_state(ctx: &Context) -> io::Result<LockState> {
    let path = coda_daemon::server::lock_path_for(&ctx.dirs.socket_path());
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LockState::Absent),
        Err(e) => return Err(e),
    };
    match file.try_lock() {
        Ok(()) => Ok(LockState::Free),
        Err(TryLockError::WouldBlock) => Ok(LockState::Held),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Whether the socket path holds a socket, or `None` when it holds nothing.
fn socket_file_is_socket(ctx: &Context) -> Option<bool> {
    let meta = ctx.dirs.socket_path().symlink_metadata().ok()?;
    Some(meta.file_type().is_socket())
}

fn check_daemon(status: &DaemonStatusResult) -> Check {
    const NAME: &str = "socket";
    let current = env!("CARGO_PKG_VERSION");
    let detail = format!(
        "daemon answering (pid {}, version {}, protocol {})",
        status.pid, status.version, status.protocol_version
    );
    if status.version == current {
        Check::ok(NAME, detail)
    } else {
        Check::warn(
            NAME,
            format!("{detail}; this is coda {current}, restart it"),
        )
    }
}

fn check_socket(ctx: &Context, daemon: Option<&DaemonStatusResult>) -> Check {
    const NAME: &str = "socket";
    if let Some(status) = daemon {
        return check_daemon(status);
    }
    match socket_file_is_socket(ctx) {
        None => Check::ok(NAME, "no daemon is running"),
        Some(false) => Check::fail(NAME, "not a socket, remove the file by hand"),
        Some(true) => match lock_state(ctx) {
            Ok(LockState::Held) => Check::warn(
                NAME,
                "a daemon holds the lock but does not answer, maybe stuck",
            ),
            _ => Check::fail(
                NAME,
                "stale socket, nothing listens; run `coda doctor --repair`",
            ),
        },
    }
}

fn check_lock(ctx: &Context, daemon: Option<&DaemonStatusResult>) -> Check {
    const NAME: &str = "lock";
    if daemon.is_some() {
        return Check::ok(NAME, "held by the running daemon");
    }
    match lock_state(ctx) {
        Ok(LockState::Absent) => Check::ok(NAME, "no lock file"),
        Ok(LockState::Held) => {
            Check::warn(NAME, "held by a process that does not answer as a daemon")
        }
        // A clean shutdown removes the socket but leaves the lock file behind.
        Ok(LockState::Free) if socket_file_is_socket(ctx) == Some(true) => Check::fail(
            NAME,
            "unheld lock beside a stale socket; run `coda doctor --repair`",
        ),
        Ok(LockState::Free) => Check::ok(NAME, "unheld lock file left by a clean shutdown"),
        Err(e) => Check::warn(NAME, format!("cannot inspect the lock: {e}")),
    }
}

async fn check_orphans(ctx: &Context, daemon: Option<&DaemonStatusResult>) -> Check {
    const NAME: &str = "orphans";
    if daemon.is_some() {
        return Check::ok(NAME, "backends belong to the running daemon");
    }
    let survey = survey_orphans(&ctx.dirs.run_dir()).await;
    if survey.live + survey.stale > 0 {
        Check::fail(
            NAME,
            format!(
                "{} orphaned backend(s) still running, {} stale pidfile(s); run `coda doctor --repair`",
                survey.live, survey.stale
            ),
        )
    } else if survey.unchecked > 0 {
        Check::warn(
            NAME,
            format!(
                "{} backend pidfile(s) could not be checked",
                survey.unchecked
            ),
        )
    } else {
        Check::ok(NAME, "no orphaned backends")
    }
}

fn check_database(ctx: &Context, daemon: Option<&DaemonStatusResult>) -> Check {
    const NAME: &str = "database";
    if daemon.is_some() {
        return Check::ok(NAME, "owned by the running daemon, integrity not checked");
    }
    match health::integrity_problems(&ctx.dirs.data_dir()) {
        Ok(None) => Check::warn(NAME, "no database yet, the daemon creates one"),
        Ok(Some(problems)) if problems.is_empty() => Check::ok(NAME, "integrity check passed"),
        Ok(Some(problems)) => Check::fail(
            NAME,
            format!("integrity check found problems, first: {}", problems[0]),
        ),
        Err(e) => Check::fail(NAME, format!("cannot read the database: {e}")),
    }
}

fn check_schema(ctx: &Context, daemon: Option<&DaemonStatusResult>) -> Check {
    const NAME: &str = "schema";
    if daemon.is_some() {
        return Check::ok(NAME, "owned by the running daemon, not read");
    }
    match health::schema_version(&ctx.dirs.data_dir()) {
        Ok(None) => Check::ok(NAME, "no database yet"),
        Ok(Some(found)) if found == SCHEMA_VERSION => Check::ok(NAME, format!("version {found}")),
        Ok(Some(found)) if found < SCHEMA_VERSION => Check::warn(
            NAME,
            format!(
                "database is at version {found}, this binary expects {SCHEMA_VERSION}; the next daemon start migrates it"
            ),
        ),
        Ok(Some(found)) => Check::fail(
            NAME,
            format!(
                "database version {found} is newer than this binary's {SCHEMA_VERSION}; upgrade coda"
            ),
        ),
        Err(_) => Check::warn(NAME, "version unreadable, see the database check"),
    }
}

fn check_release(ctx: &Context, daemon: Option<&DaemonStatusResult>) -> Check {
    const NAME: &str = "database release";
    if daemon.is_some() {
        return Check::ok(NAME, "owned by the running daemon, not read");
    }
    match health::release_status(&ctx.dirs.data_dir()) {
        Ok(None) => Check::ok(NAME, "no database yet"),
        Ok(Some(DatabaseRelease::Current)) => Check::ok(NAME, format!("release {BINARY_RELEASE}")),
        Ok(Some(DatabaseRelease::Unrecorded)) => Check::warn(
            NAME,
            format!(
                "database predates release tracking, will migrate on next start to {BINARY_RELEASE}"
            ),
        ),
        Ok(Some(DatabaseRelease::Older(found))) => Check::warn(
            NAME,
            format!(
                "database was last opened by release {found}, will migrate on next start to {BINARY_RELEASE}"
            ),
        ),
        Ok(Some(DatabaseRelease::Newer(found))) => Check::fail(
            NAME,
            format!(
                "database was last opened by release {found}, newer than this binary's {BINARY_RELEASE}; upgrade coda"
            ),
        ),
        Err(e) => Check::fail(NAME, e.to_string()),
    }
}

/// Parses the config without echoing its text: a parse error quotes the
/// offending line, which may hold a secret, so only its position is shown.
fn check_config(path: &Path) -> (Check, Option<Config>) {
    const NAME: &str = "config";
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let ok = Check::ok(NAME, "no config file, defaults in use");
            return (ok, Some(Config::default()));
        }
        Err(e) => {
            let fail = Check::fail(NAME, format!("cannot read the config: {e}"));
            return (fail, None);
        }
    };
    match Config::from_toml_str(&text) {
        Ok(parsed) => {
            let mut notes = Vec::new();
            if !parsed.unknown_keys.is_empty() {
                notes.push(format!(
                    "unknown keys ignored: {}",
                    parsed.unknown_keys.join(", ")
                ));
            }
            notes.extend(routing_notes(&parsed.config));
            let check = if notes.is_empty() {
                Check::ok(NAME, "valid")
            } else {
                Check::warn(NAME, notes.join("; "))
            };
            (check, Some(parsed.config))
        }
        Err(ConfigError::Syntax(e)) => {
            let position = e.span().map_or_else(String::new, |span| {
                let line = text[..span.start.min(text.len())].matches('\n').count() + 1;
                format!(" near line {line}")
            });
            let detail = format!("not valid for the config schema{position}");
            (Check::fail(NAME, detail), None)
        }
        Err(e) => (Check::fail(NAME, e.to_string()), None),
    }
}

/// The model-API provider a session starts on by default: the backend
/// `coda run` picks. `None` when that is Claude Code or Codex, which run their
/// own tools and no `task` call.
fn default_session_provider(config: &Config) -> Option<&'static str> {
    match pick_backend(config) {
        BackendKind::Faux => Some(FAUX_KEY),
        // `pick_backend` never picks the OpenAI-compatible provider: with none
        // of the others enabled it falls back to Anthropic.
        BackendKind::Anthropic => Some(ANTHROPIC_KEY),
        _ => None,
    }
}

/// A warning for each `[routing]` alias whose provider is not the default
/// session provider: a `task` child runs on its session's provider, so a
/// session on the default one cannot use that alias.
fn routing_notes(config: &Config) -> Vec<String> {
    let Some(default) = default_session_provider(config) else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    for (tier, alias) in &config.routing.0 {
        let provider = config
            .models
            .get(alias)
            .map(|model| model.provider.as_str());
        if let Some(provider) = provider.filter(|provider| *provider != default) {
            notes.push(format!(
                "[routing] {tier} names `{alias}`, which uses {provider}, but the default session \
                 provider is {default}; a `task` call in a session on {default} cannot use it"
            ));
        }
    }
    notes
}

async fn check_backends(config: Option<Config>) -> Check {
    const NAME: &str = "backends";
    let Some(config) = config else {
        return Check::warn(NAME, "skipped, the config is not valid");
    };
    let search_dirs: Vec<_> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    // Probing runs `--version` and waits on it, so it stays off the runtime.
    let probed = tokio::task::spawn_blocking(move || {
        let backends: [&dyn AgentBackend; 2] = [&ClaudeCodeBackend, &CodexBackend];
        backends
            .into_iter()
            .map(|backend| probe_backend(backend, &config, &search_dirs))
            .collect::<Vec<_>>()
    })
    .await;
    match probed {
        Ok(parts) => {
            let status = parts.iter().map(|(s, _)| *s).max().unwrap_or(Status::Ok);
            let detail = parts.into_iter().map(|(_, d)| d).collect::<Vec<_>>();
            Check::new(NAME, status, detail.join("; "))
        }
        Err(e) => Check::warn(NAME, format!("the probe did not finish: {e}")),
    }
}

fn probe_backend(
    backend: &dyn AgentBackend,
    config: &Config,
    search_dirs: &[PathBuf],
) -> (Status, String) {
    let key = backend.config_key();
    if require_enabled(backend, config).is_err() {
        return (Status::Ok, format!("{key} disabled"));
    }
    match locate(backend, config, search_dirs) {
        Ok(found) if found.verified => (
            Status::Ok,
            format!("{key} {} at {}", found.version, found.path.display()),
        ),
        Ok(found) => (
            Status::Warn,
            format!(
                "{key} version {} is not on the verified list (drift); add it to verified_versions once re-checked",
                found.version
            ),
        ),
        Err(e) => (Status::Fail, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Report {
        Report::new(
            vec![
                Check::new("socket", Status::Ok, "no daemon is running"),
                Check::new("database", Status::Warn, "no database yet"),
                Check::new("config", Status::Fail, "bad"),
            ],
            vec!["removed a stale socket".to_owned()],
        )
    }

    #[test]
    fn the_json_report_has_a_stable_shape() {
        // Arrange
        let report = sample();

        // Act
        let json = serde_json::to_string(&report).unwrap();

        // Assert
        assert_eq!(
            json,
            r#"{"healthy":false,"checks":[{"name":"socket","status":"ok","detail":"no daemon is running"},{"name":"database","status":"warn","detail":"no database yet"},{"name":"config","status":"fail","detail":"bad"}],"repairs":["removed a stale socket"]}"#
        );
    }

    #[test]
    fn the_worst_status_picks_the_exit_code() {
        for (worst, code) in [
            (Status::Ok, ExitCode::SUCCESS),
            (Status::Warn, ExitCode::from(exit::DOCTOR_WARNINGS)),
            (Status::Fail, ExitCode::from(exit::DOCTOR_FAILURES)),
        ] {
            // Arrange
            let checks = vec![Check::new("a", Status::Ok, ""), Check::new("b", worst, "")];

            // Act
            let report = Report::new(checks, Vec::new());

            // Assert
            assert_eq!(report.exit_code(), code);
        }
    }

    #[test]
    fn the_table_aligns_name_status_and_detail() {
        // Arrange
        let report = Report::new(
            vec![Check::new("socket", Status::Ok, "fine")],
            vec!["removed a stale socket".to_owned()],
        );
        let mut out = Vec::new();

        // Act
        report.write_table(&mut out).unwrap();

        // Assert
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "socket    ok    fine\nrepaired: removed a stale socket\n"
        );
    }

    #[test]
    fn a_config_parse_error_shows_its_line_but_not_its_text() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "max_turns = 1\nmax_turns = \"hunter2\"\n").unwrap();

        // Act
        let (check, config) = check_config(&path);

        // Assert
        assert_eq!(check.status, Status::Fail);
        assert!(!check.detail.contains("hunter2"));
        assert!(config.is_none());
    }

    const ANTHROPIC: &str = "[providers.anthropic]\nenabled = true\n";
    const OPENAI: &str =
        "[providers.openai_compat]\nenabled = true\nbase_url = \"http://localhost:1/v1\"\n";
    const QUICK: &str = "[models.quick]\nprovider = \"anthropic\"\nmodel = \"m\"\n";
    const GPT: &str = "[models.gpt]\nprovider = \"openai_compat\"\nmodel = \"m\"\n";

    fn checked(toml: &str) -> Check {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, toml).unwrap();
        check_config(&path).0
    }

    #[test]
    fn a_routed_alias_of_another_provider_than_the_default_session_provider_warns() {
        // Arrange
        let away = format!("{ANTHROPIC}{OPENAI}{GPT}[routing]\ndeep = \"gpt\"\n");
        let also_unknown = format!("surprise = 1\n{away}");

        // Act
        let (warned, both) = (checked(&away), checked(&also_unknown));

        // Assert
        assert_eq!(warned.status, Status::Warn, "{}", warned.detail);
        for word in ["[routing] deep", "`gpt`", "openai_compat", "anthropic"] {
            assert!(warned.detail.contains(word), "{word}: {}", warned.detail);
        }
        assert_eq!(both.status, Status::Warn);
        assert!(both.detail.contains("surprise") && both.detail.contains("[routing] deep"));
    }

    #[test]
    fn a_config_with_only_the_openai_compatible_provider_still_starts_on_anthropic() {
        // Arrange
        // `coda run` never starts an OpenAI-compatible session: with no other
        // provider enabled it falls back to Anthropic.
        let only_openai = format!("{OPENAI}{GPT}[routing]\ndeep = \"gpt\"\n");

        // Act
        let check = checked(&only_openai);

        // Assert
        assert_eq!(check.status, Status::Warn, "{}", check.detail);
        assert!(
            check
                .detail
                .contains("default session provider is anthropic"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn a_routed_alias_the_default_session_can_use_is_not_a_warning() {
        for (name, toml) in [
            (
                "same provider",
                format!("{ANTHROPIC}{QUICK}[routing]\nfast = \"quick\"\n"),
            ),
            (
                "an agent backend is the default and runs no task call",
                format!(
                    "[providers.claude_code]\nenabled = true\n{QUICK}[routing]\nfast = \"quick\"\n"
                ),
            ),
        ] {
            // Act
            let check = checked(&toml);

            // Assert
            assert_eq!(check.status, Status::Ok, "{name}: {}", check.detail);
        }
    }

    #[test]
    fn a_daemon_of_another_version_is_a_warning() {
        for (version, expected) in [
            (env!("CARGO_PKG_VERSION"), Status::Ok),
            ("0.0.0-other", Status::Warn),
        ] {
            // Arrange
            let json = format!(
                r#"{{"pid":1,"protocol_version":1,"version":"{version}","uptime_secs":0,"client_count":1,"active_turns":0,"data_dir":""}}"#
            );
            let status = serde_json::from_str(&json).unwrap();

            // Act
            let check = check_daemon(&status);

            // Assert
            assert_eq!(check.status, expected);
        }
    }
}
