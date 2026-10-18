// SPDX-License-Identifier: AGPL-3.0-only

//! Registration of the Claude Code session adapter with the daemon.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use coda_config::config::Config;
use coda_protocol::types::{BackendKind, SessionId};
use coda_providers::backend::{
    AgentBackend, BackendError, VERIFIED_CLAUDE_CODE_VERSIONS, locate, require_enabled,
};
use coda_providers::claude_session::{AdapterError, ClaudeSession, SessionSpec};
use coda_providers::detect::DetectedBinary;

use crate::pidfile::{self, PidfileError};

/// Extension of the pidfile written next to each running child.
pub const PIDFILE_EXTENSION: &str = "pid";

const CONFIG_KEY: &str = "claude_code";
const BINARY_NAME: &str = "claude";

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

/// The delegated agent backends the daemon registers; a Codex backend joins here.
const AGENT_BACKENDS: &[&dyn AgentBackend] = &[&ClaudeCodeBackend];

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

/// The pidfile path of `session` under `dir`.
#[must_use]
pub fn pidfile_path(dir: &Path, session: &SessionId) -> PathBuf {
    dir.join(format!("{}.{PIDFILE_EXTENSION}", session.as_str()))
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use coda_protocol::types::{BackendKind, SessionId};
    use coda_providers::backend::AgentBackend;

    use coda_config::config::Config;

    use super::{
        ClaudeCodeBackend, SessionStart, StartError, enabled_agent_backends, pidfile_path,
        start_claude_session,
    };
    use crate::pidfile::{is_same_process, read};

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
}
