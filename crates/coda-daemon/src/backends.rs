// SPDX-License-Identifier: AGPL-3.0-only

//! Registration of the delegated agent backends with the daemon, and how their
//! children are started with a pidfile the startup reaper can find.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use coda_config::config::Config;
use coda_protocol::events::Event;
use coda_protocol::types::{BackendKind, SessionId, TurnId};
use coda_providers::backend::{
    AgentBackend, BackendError, VERIFIED_CLAUDE_CODE_VERSIONS, VERIFIED_CODEX_VERSIONS, locate,
    locate_within, require_enabled,
};
use coda_providers::claude_session::{
    AdapterError, CancelHandle, ClaudeSession, SessionSpec, TurnOutcome,
};
use coda_providers::codex::{
    Carried, CodexLaunch, CodexSession, CodexSessionError, SandboxError, SandboxMode, SpawnHook,
    resolve_sandbox,
};
use coda_providers::detect::DetectedBinary;
use coda_providers::gate::PermissionGate;

use crate::pidfile::{self, PidfileError};

/// Extension of the pidfile written next to each running child.
pub const PIDFILE_EXTENSION: &str = "pid";

const CONFIG_KEY: &str = "claude_code";
const BINARY_NAME: &str = "claude";
const CODEX_CONFIG_KEY: &str = "codex";
const CODEX_BINARY_NAME: &str = "codex";

/// Claude Code as the daemon's delegated agent.
#[derive(Debug, Clone, Copy)]
pub struct ClaudeCodeBackend;

impl AgentBackend for ClaudeCodeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::ClaudeCode
    }
    fn config_key(&self) -> &'static str {
        CONFIG_KEY
    }
    fn binary_name(&self) -> &'static str {
        BINARY_NAME
    }
    fn verified_versions(&self) -> &'static [&'static str] {
        VERIFIED_CLAUDE_CODE_VERSIONS
    }
}

/// Codex as a delegated agent. It registers as the app-server backend; a session
/// that falls back to `exec` reports [`BackendKind::CodexExec`] itself.
#[derive(Debug, Clone, Copy)]
pub struct CodexBackend;

impl AgentBackend for CodexBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::CodexAppServer
    }
    fn config_key(&self) -> &'static str {
        CODEX_CONFIG_KEY
    }
    fn binary_name(&self) -> &'static str {
        CODEX_BINARY_NAME
    }
    fn verified_versions(&self) -> &'static [&'static str] {
        VERIFIED_CODEX_VERSIONS
    }
}

/// The delegated agent backends the daemon registers.
const AGENT_BACKENDS: &[&dyn AgentBackend] = &[&ClaudeCodeBackend, &CodexBackend];

/// The registered agent backends that `config` enables.
#[must_use]
pub fn enabled_agent_backends(config: &Config) -> Vec<BackendKind> {
    AGENT_BACKENDS
        .iter()
        .filter(|backend| require_enabled(**backend, config).is_ok())
        .map(|backend| backend.kind())
        .collect()
}

/// Finds the Claude Code binary once the backend is enabled in `config`.
///
/// # Errors
/// Returns the [`BackendError`] of [`locate`].
pub fn locate_claude_code(
    config: &Config,
    search_dirs: &[PathBuf],
) -> Result<DetectedBinary, BackendError> {
    locate(&ClaudeCodeBackend, config, search_dirs)
}

/// Why a session could not be started.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Spawn(#[from] AdapterError),
    #[error(transparent)]
    Pidfile(#[from] PidfileError),
    #[error("the started child has no process id, so it exited before its pidfile was written")]
    NoPid,
    #[error("the pidfile failed ({pidfile}) and the child could not be stopped: {source}")]
    Cleanup {
        pidfile: String,
        source: AdapterError,
    },
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    #[error(transparent)]
    Codex(#[from] CodexSessionError),
    #[error("the Codex version check could not run: {0}")]
    Probe(#[from] tokio::task::JoinError),
}

/// What it takes to start one Claude Code session.
#[derive(Debug, Clone)]
pub struct SessionStart {
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub session_id: SessionId,
    pub resume: Option<String>,
    pub parent_env: Vec<(OsString, OsString)>,
    /// Directory that holds the child's pidfile.
    pub pidfile_dir: PathBuf,
}

/// Starts the child and records its pidfile; the child is stopped again if
/// the pidfile cannot be written.
///
/// # Errors
/// Returns [`StartError::Spawn`] when the binary cannot start,
/// [`StartError::Pidfile`] or [`StartError::NoPid`] when the pidfile cannot be
/// written, and [`StartError::Cleanup`] when the child then cannot be stopped.
pub async fn start_claude_session(start: SessionStart) -> Result<ClaudeSession, StartError> {
    let path = pidfile_path(&start.pidfile_dir, &start.session_id);
    let mut session = ClaudeSession::spawn(SessionSpec {
        binary: start.binary.clone(),
        cwd: start.cwd,
        session_id: start.session_id,
        resume: start.resume,
        parent_env: start.parent_env,
    })?;
    let written = match session.pid() {
        Some(pid) => pidfile::write(&path, pid, &start.binary)
            .await
            .map_err(StartError::from),
        None => Err(StartError::NoPid),
    };
    if let Err(e) = written {
        return Err(match session.shutdown().await {
            Ok(()) => e,
            Err(source) => StartError::Cleanup {
                pidfile: e.to_string(),
                source,
            },
        });
    }
    Ok(session)
}

/// What a Codex session runs with, resolved from the `providers.codex` config.
#[derive(Debug, Clone)]
pub struct CodexSettings {
    pub binary: PathBuf,
    pub sandbox: SandboxMode,
    pub verified_versions: Vec<String>,
    /// Whether the binary's own `--version` is on the verified list.
    pub binary_verified: bool,
}

/// Finds the Codex binary and the sandbox its sessions run under; `--version`
/// may take up to `probe_timeout`.
///
/// # Errors
/// Returns [`StartError::Backend`] when Codex is disabled or not installed and
/// [`StartError::Sandbox`] when the sandbox is wider than read-only without
/// `allow_wider_sandbox`.
pub fn resolve_codex(
    config: &Config,
    search_dirs: &[PathBuf],
    probe_timeout: Duration,
) -> Result<CodexSettings, StartError> {
    let binary = locate_within(&CodexBackend, config, search_dirs, probe_timeout)?;
    let section = require_enabled(&CodexBackend, config)?;
    let sandbox = resolve_sandbox(section.sandbox.as_deref(), section.allow_wider_sandbox)?;
    let verified_versions = CodexBackend
        .verified_versions()
        .iter()
        .map(|version| (*version).to_owned())
        .chain(section.verified_versions.iter().cloned())
        .collect();
    Ok(CodexSettings {
        binary: binary.path,
        sandbox,
        verified_versions,
        binary_verified: binary.verified,
    })
}

/// [`resolve_codex`] on a blocking thread, because probing `--version` waits on a
/// child process and must not hold up a runtime worker.
///
/// # Errors
/// Returns what [`resolve_codex`] returns, or [`StartError::Probe`] when the
/// blocking task does not finish.
pub async fn resolve_codex_off_worker(
    config: Config,
    search_dirs: Vec<PathBuf>,
    probe_timeout: Duration,
) -> Result<CodexSettings, StartError> {
    tokio::task::spawn_blocking(move || resolve_codex(&config, &search_dirs, probe_timeout)).await?
}

/// What it takes to start one Codex session.
#[derive(Debug, Clone)]
pub struct CodexStart {
    pub settings: CodexSettings,
    pub cwd: PathBuf,
    pub session_id: SessionId,
    pub resume: Option<String>,
    /// The earlier conversation, which a new app-server thread cannot see.
    pub carried: Carried,
    pub parent_env: Vec<(OsString, OsString)>,
    /// Directory that holds the child's pidfile.
    pub pidfile_dir: PathBuf,
}

/// Starts the Codex session, recording a pidfile for every child it spawns
/// (the app-server once, the exec fallback on each turn).
///
/// # Errors
/// Returns [`StartError::Codex`] when the session cannot start, including when
/// a pidfile cannot be written.
pub async fn start_codex_session(start: CodexStart) -> Result<CodexSession, StartError> {
    let path = pidfile_path(&start.pidfile_dir, &start.session_id);
    let exe = start.settings.binary.clone();
    let record_pid: SpawnHook = Arc::new(move |pid| {
        let (path, exe) = (path.clone(), exe.clone());
        Box::pin(async move {
            pidfile::write(&path, pid, &exe)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    });
    let launch = CodexLaunch {
        binary: start.settings.binary,
        cwd: start.cwd,
        session_id: start.session_id,
        resume: start.resume,
        sandbox: start.settings.sandbox,
        verified_versions: start.settings.verified_versions,
        binary_verified: start.settings.binary_verified,
        carried: start.carried,
        parent_env: start.parent_env,
    };
    Ok(CodexSession::start(launch, Some(record_pid)).await?)
}

/// The session of whichever agent backend a turn runs on.
#[derive(Debug)]
pub enum AgentSession {
    Claude(Box<ClaudeSession>),
    Codex(Box<CodexSession>),
}

/// Why a turn on an [`AgentSession`] failed.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Claude(#[from] AdapterError),
    #[error(transparent)]
    Codex(#[from] CodexSessionError),
}

impl AgentSession {
    /// A handle that cancels the session from another task.
    #[must_use]
    pub fn cancel_handle(&self) -> CancelHandle {
        match self {
            Self::Claude(session) => session.cancel_handle(),
            Self::Codex(session) => session.cancel_handle(),
        }
    }

    /// The backend child's process id, until it has been reaped.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        match self {
            Self::Claude(session) => session.pid(),
            Self::Codex(session) => session.pid(),
        }
    }

    /// Whether the session is a running server that can take another turn.
    pub fn is_reusable(&mut self) -> bool {
        match self {
            Self::Claude(_) => false,
            Self::Codex(session) => session.is_reusable(),
        }
    }

    /// Whether a crashed turn may be started again from its prompt.
    #[must_use]
    pub fn restartable(&self) -> bool {
        match self {
            Self::Claude(_) => true,
            Self::Codex(session) => session.restartable(),
        }
    }

    /// The backend's own conversation id, so the next turn can resume it.
    #[must_use]
    pub fn session_id_from_backend(&self) -> Option<&str> {
        match self {
            Self::Claude(session) => session.session_id_from_backend(),
            Self::Codex(session) => session.session_id_from_backend(),
        }
    }

    /// Runs one prompt to the end of the turn.
    ///
    /// # Errors
    /// Returns the error of the backend that ran the turn.
    pub async fn run_turn<G: PermissionGate>(
        &mut self,
        turn_id: &TurnId,
        text: &str,
        gate: &G,
        emit: &mut impl FnMut(Event),
    ) -> Result<TurnOutcome, AgentError> {
        Ok(match self {
            Self::Claude(session) => session.run_turn(turn_id, text, gate, emit).await?,
            Self::Codex(session) => session.run_turn(turn_id, text, gate, emit).await?,
        })
    }

    /// Stops the backend's process group.
    ///
    /// # Errors
    /// Returns the error of the backend.
    pub async fn shutdown(&mut self) -> Result<(), AgentError> {
        match self {
            Self::Claude(session) => session.shutdown().await?,
            Self::Codex(session) => session.shutdown().await?,
        }
        Ok(())
    }
}

/// The pidfile path of `session` under `dir`.
#[must_use]
pub fn pidfile_path(dir: &Path, session: &SessionId) -> PathBuf {
    dir.join(format!("{}.{PIDFILE_EXTENSION}", session.as_str()))
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use coda_protocol::types::{BackendKind, SessionId};
    use coda_providers::backend::AgentBackend;
    use coda_providers::codex::{Carried, SandboxError};
    use coda_providers::codex_exec::SandboxMode;
    use coda_store::Store;

    use coda_config::config::Config;

    use super::{
        ClaudeCodeBackend, CodexStart, SessionStart, StartError, enabled_agent_backends,
        pidfile_path, resolve_codex, resolve_codex_off_worker, start_claude_session,
        start_codex_session,
    };
    use crate::pidfile::{is_same_process, read};
    use crate::reaper::reap;
    use crate::subscribe::{DEFAULT_QUEUE_CAPACITY, Hub};

    const WAIT: Duration = Duration::from_secs(30);
    /// An upper bound, not a duration: starting a freshly written script can
    /// stall for many seconds on a loaded machine.
    const PATIENT_PROBE: Duration = Duration::from_secs(60);
    const TICK: Duration = Duration::from_millis(10);
    const TICKS_DURING_PROBE: u32 = 5;
    const MAX_HOLD_POLLS: u32 = 3_000;

    fn start(dir: &tempfile::TempDir, binary: std::path::PathBuf) -> SessionStart {
        SessionStart {
            binary,
            cwd: dir.path().to_path_buf(),
            session_id: SessionId::new("s-1"),
            resume: None,
            parent_env: Vec::new(),
            pidfile_dir: dir.path().to_path_buf(),
        }
    }

    #[test]
    fn claude_code_is_listed_only_when_its_section_is_enabled() {
        // Arrange
        let enabled = Config::from_toml_str("[providers.claude_code]\nenabled = true\n")
            .unwrap()
            .config;
        let disabled = Config::from_toml_str("[providers.claude_code]\nenabled = false\n")
            .unwrap()
            .config;

        // Act
        let (on, off) = (
            enabled_agent_backends(&enabled),
            enabled_agent_backends(&disabled),
        );

        // Assert
        assert_eq!(on, [BackendKind::ClaudeCode]);
        assert_eq!(off, Vec::<BackendKind>::new());
    }

    #[test]
    fn the_backend_is_claude_code_with_its_config_key_and_binary() {
        // Arrange
        let backend = ClaudeCodeBackend;

        // Act
        let identity = (backend.kind(), backend.config_key(), backend.binary_name());

        // Assert
        assert_eq!(identity, (BackendKind::ClaudeCode, "claude_code", "claude"));
    }

    #[tokio::test]
    async fn a_started_session_has_an_owner_only_pidfile_for_its_child() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let binary = coda_testkit::paths::fake_claude_path().unwrap();

        // Act
        let session = start_claude_session(start(&dir, binary.clone()))
            .await
            .unwrap();

        // Assert
        let path = pidfile_path(dir.path(), &SessionId::new("s-1"));
        let record = read(&path).unwrap();
        assert_eq!(Some(record.pid), session.pid());
        assert_eq!(record.exe, binary);
        assert!(is_same_process(&record).await);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn an_unwritable_pidfile_dir_is_a_pidfile_error() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let binary = coda_testkit::paths::fake_claude_path().unwrap();
        let mut spec = start(&dir, binary);
        spec.pidfile_dir = dir.path().join("missing");

        // Act
        let result = start_claude_session(spec).await;

        // Assert
        assert!(matches!(result, Err(StartError::Pidfile(_))));
    }

    #[tokio::test]
    async fn a_missing_binary_is_a_spawn_error_and_leaves_no_pidfile() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-claude");

        // Act
        let result = start_claude_session(start(&dir, missing)).await;

        // Assert
        assert!(matches!(result, Err(StartError::Spawn(_))));
        assert!(!pidfile_path(dir.path(), &SessionId::new("s-1")).exists());
    }

    fn codex_config(extra: &str) -> Config {
        let binary = coda_testkit::paths::fake_codex_path().unwrap();
        let toml = format!(
            "[providers.codex]\nenabled = true\nbinary = \"{}\"\nverified_versions = [\"0.0.0-fake\"]\n{extra}",
            binary.display()
        );
        Config::from_toml_str(&toml).unwrap().config
    }

    fn codex_start(dir: &tempfile::TempDir, config: &Config) -> CodexStart {
        CodexStart {
            settings: resolve_codex(config, &[], PATIENT_PROBE).unwrap(),
            cwd: dir.path().to_path_buf(),
            session_id: SessionId::new("s-1"),
            resume: None,
            carried: Carried::Nothing,
            parent_env: Vec::new(),
            pidfile_dir: dir.path().to_path_buf(),
        }
    }

    fn control(dir: &tempfile::TempDir, value: &serde_json::Value) {
        fs::write(
            dir.path().join("fake-codex-control.json"),
            value.to_string(),
        )
        .unwrap();
    }

    #[test]
    fn codex_is_registered_only_when_its_section_is_enabled() {
        // Arrange
        let both = Config::from_toml_str(
            "[providers.claude_code]\nenabled = true\n[providers.codex]\nenabled = true\n",
        )
        .unwrap()
        .config;
        let disabled = Config::from_toml_str("[providers.codex]\nenabled = false\n")
            .unwrap()
            .config;

        // Act
        let (on, off) = (
            enabled_agent_backends(&both),
            enabled_agent_backends(&disabled),
        );

        // Assert
        assert_eq!(on, [BackendKind::ClaudeCode, BackendKind::CodexAppServer]);
        assert_eq!(off, Vec::<BackendKind>::new());
    }

    #[test]
    fn a_wider_sandbox_is_refused_without_the_opt_in_and_accepted_with_it() {
        // Arrange
        let refused = codex_config("sandbox = \"workspace-write\"\n");
        let accepted = codex_config("sandbox = \"workspace-write\"\nallow_wider_sandbox = true\n");
        let default = codex_config("");

        // Act
        let (refused, accepted, default) = (
            resolve_codex(&refused, &[], PATIENT_PROBE),
            resolve_codex(&accepted, &[], PATIENT_PROBE),
            resolve_codex(&default, &[], PATIENT_PROBE),
        );

        // Assert
        assert!(matches!(
            refused,
            Err(StartError::Sandbox(SandboxError::WiderRefused { .. }))
        ));
        assert_eq!(accepted.unwrap().sandbox, SandboxMode::WorkspaceWrite);
        let default = default.unwrap();
        assert_eq!(default.sandbox, SandboxMode::ReadOnly);
        assert!(
            default.binary_verified,
            "a prerelease version is matched whole"
        );
    }

    #[tokio::test]
    async fn a_codex_app_server_has_an_owner_only_pidfile_until_it_stops() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let config = codex_config("");

        // Act
        let mut session = start_codex_session(codex_start(&dir, &config))
            .await
            .unwrap();

        // Assert
        let path = pidfile_path(dir.path(), &SessionId::new("s-1"));
        let record = read(&path).unwrap();
        assert_eq!(session.kind(), BackendKind::CodexAppServer);
        assert_eq!(Some(record.pid), session.pid());
        assert!(is_same_process(&record).await);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_server_that_never_finishes_starting_still_has_a_pidfile_the_reaper_kills() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        control(&dir, &serde_json::json!({"hang_start": true}));
        let config = codex_config("");
        let starting = tokio::spawn(start_codex_session(codex_start(&dir, &config)));
        let path = pidfile_path(dir.path(), &SessionId::new("s-1"));
        tokio::time::timeout(WAIT, async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("no pidfile appeared while the server was starting");
        let hub = Hub::new(
            Store::open(&dir.path().join("store")).unwrap(),
            DEFAULT_QUEUE_CAPACITY,
        );

        // Act
        let report = reap(dir.path(), &hub).await;
        let result = tokio::time::timeout(WAIT, starting)
            .await
            .expect("the killed server did not end the start in time")
            .unwrap();

        // Assert
        assert_eq!(report.killed, 1);
        assert!(!path.exists());
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn a_slow_version_probe_does_not_block_other_tasks() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("codex");
        let release = dir.path().join("release");
        // The probe stays open until the ticker has run, so the test never
        // depends on how long the probe happens to take. The loop is bounded so
        // a probe that blocks the runtime still ends, and then fails the test.
        let script = format!(
            "#!/bin/sh\nn=0\nwhile [ ! -e '{}' ] && [ $n -lt {MAX_HOLD_POLLS} ]; do\n  sleep 0.01\n  n=$((n+1))\ndone\necho 'codex-cli 1.0.0'\n",
            release.display()
        );
        fs::write(&binary, script).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let toml = format!(
            "[providers.codex]\nenabled = true\nbinary = \"{}\"\n",
            binary.display()
        );
        let config = Config::from_toml_str(&toml).unwrap().config;

        // Act
        let settings = tokio::time::timeout(WAIT, async {
            let probe = resolve_codex_off_worker(config, Vec::new(), PATIENT_PROBE);
            tokio::pin!(probe);
            for _ in 0..TICKS_DURING_PROBE {
                tokio::select! {
                    settings = &mut probe => panic!("the probe ended before it was released: {settings:?}"),
                    () = tokio::time::sleep(TICK) => {}
                }
            }
            fs::write(&release, "").unwrap();
            probe.await
        })
        .await
        .expect("the probe, or the runtime beside it, stalled");

        // Assert
        assert_eq!(settings.unwrap().binary, binary);
    }
}
