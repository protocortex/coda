// SPDX-License-Identifier: AGPL-3.0-only

//! Chooses between the Codex `app-server` and `exec` backends for one session.
//!
//! The app-server is preferred because it can ask for approvals. Coda falls back to
//! `exec` only when the binary's `--version` is not on the verified list (its
//! server cannot start, or reports a version outside the list), always in the
//! read-only sandbox and with a visible `approvals_unavailable` event. Any other
//! failure, including a verified server that cannot start or a crash mid-turn, is
//! reported and never retried on the other backend.

use std::ffi::OsString;
use std::path::PathBuf;

use coda_protocol::events::Event;
use coda_protocol::types::{BackendKind, SessionId, TurnId};
use tracing::{info, warn};

use crate::claude_session::{CancelHandle, TurnOutcome};
use crate::codex_app_server::{AppServerError, AppServerSpec, CodexAppServerSession};
use crate::codex_exec::{CodexError, CodexExecSession, CodexSpec};
use crate::gate::PermissionGate;

pub use crate::codex_exec::{SandboxMode, SpawnHook};

/// Coda keeps its own history, so the app-server thread stays out of Codex's.
const EPHEMERAL_THREAD: bool = true;
/// Each app-server turn opens a new thread, so earlier turns are not visible to it.
const NOT_CARRIED_NOTICE: &str = "Codex app-server turns do not carry over the earlier conversation yet: this turn starts a new thread.";
const SANDBOX_READ_ONLY: &str = "read-only";
const SANDBOX_WORKSPACE_WRITE: &str = "workspace-write";
const SANDBOX_FULL_ACCESS: &str = "danger-full-access";

/// Why a configured sandbox cannot be used.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SandboxError {
    /// A sandbox wider than read-only was requested without the opt-in.
    #[error(
        "sandbox `{requested}` is wider than read-only; set allow_wider_sandbox = true under providers.codex to use it"
    )]
    WiderRefused {
        /// The sandbox that was asked for.
        requested: String,
    },
    /// The name is not a Codex sandbox.
    #[error(
        "unknown sandbox `{requested}`; use {SANDBOX_READ_ONLY}, {SANDBOX_WORKSPACE_WRITE} or {SANDBOX_FULL_ACCESS}"
    )]
    Unknown {
        /// The name that was given.
        requested: String,
    },
}

/// The sandbox to run under: read-only unless `requested` names a wider one and
/// `allow_wider` is set.
///
/// # Errors
/// Returns [`SandboxError::WiderRefused`] for a wider sandbox without the opt-in
/// and [`SandboxError::Unknown`] for a name Codex does not have.
pub fn resolve_sandbox(
    requested: Option<&str>,
    allow_wider: bool,
) -> Result<SandboxMode, SandboxError> {
    let mode = match requested {
        None | Some(SANDBOX_READ_ONLY) => return Ok(SandboxMode::ReadOnly),
        Some(SANDBOX_WORKSPACE_WRITE) => SandboxMode::WorkspaceWrite,
        Some(SANDBOX_FULL_ACCESS) => SandboxMode::DangerFullAccess,
        Some(other) => {
            return Err(SandboxError::Unknown {
                requested: other.to_owned(),
            });
        }
    };
    if allow_wider {
        Ok(mode)
    } else {
        Err(SandboxError::WiderRefused {
            requested: mode.as_arg().to_owned(),
        })
    }
}

/// Why a Codex session could not start or run a turn.
#[derive(Debug, thiserror::Error)]
pub enum CodexSessionError {
    /// The app-server failed in a way that is not "absent".
    #[error(transparent)]
    AppServer(#[from] AppServerError),
    /// The exec backend failed.
    #[error(transparent)]
    Exec(#[from] CodexError),
}

/// What it takes to start a Codex session.
#[derive(Debug, Clone)]
pub struct CodexLaunch {
    /// The Codex executable.
    pub binary: PathBuf,
    /// Working directory of the child.
    pub cwd: PathBuf,
    /// The coda session the events belong to.
    pub session_id: SessionId,
    /// Codex thread id the exec fallback continues; the app-server starts a new thread.
    pub resume: Option<String>,
    /// Sandbox the app-server thread runs under; the fallback is always read-only.
    pub sandbox: SandboxMode,
    /// App-server versions known to behave; any other one falls back to exec.
    /// Matched as whole strings, like the `--version` check.
    pub verified_versions: Vec<String>,
    /// Whether the binary's own `--version` is on the verified list. Only then is
    /// a server that cannot start an error instead of a reason to fall back.
    pub binary_verified: bool,
    /// Whether the conversation already has earlier turns, which a new app-server
    /// thread cannot see.
    pub continued: bool,
    /// The parent environment; only allowlisted names reach the child.
    pub parent_env: Vec<(OsString, OsString)>,
}

enum Active {
    AppServer(Box<CodexAppServerSession>),
    Exec(Box<CodexExecSession>),
}

/// A Codex session on whichever backend was selected.
pub struct CodexSession {
    active: Active,
    session_id: SessionId,
    /// Why approvals are unavailable; emitted once, at the start of the first turn.
    notice: Option<String>,
    /// Tells the user the app-server thread does not hold the earlier turns.
    not_carried: bool,
}

impl std::fmt::Debug for CodexSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexSession")
            .field("kind", &self.kind())
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl CodexSession {
    /// Starts the app-server, or the read-only exec fallback when the server is
    /// cannot start or reports an unverified version while the binary is not
    /// verified. `on_spawn` sees every child's pid as soon as it starts.
    ///
    /// # Errors
    /// Returns the [`AppServerError`] of a server that is broken, or of a verified
    /// binary that cannot start, since that is not a reason to switch backend. The
    /// server is stopped on every error, including when `on_spawn` fails.
    pub async fn start(
        launch: CodexLaunch,
        on_spawn: Option<SpawnHook>,
    ) -> Result<Self, CodexSessionError> {
        info!(step = "start_app_server", "starting the Codex app-server");
        let spec = AppServerSpec {
            binary: launch.binary.clone(),
            cwd: launch.cwd.clone(),
            session_id: launch.session_id.clone(),
            sandbox: launch.sandbox,
            ephemeral: EPHEMERAL_THREAD,
            verified_versions: launch.verified_versions.clone(),
            parent_env: launch.parent_env.clone(),
        };
        match CodexAppServerSession::start(spec, on_spawn.clone()).await {
            Ok(server) if server.drift_warning().is_none() => Ok(Self {
                active: Active::AppServer(Box::new(server)),
                session_id: launch.session_id,
                notice: None,
                not_carried: launch.continued,
            }),
            Ok(mut server) => {
                let version = server.server_version().to_owned();
                server.shutdown().await?;
                let shown = if version.is_empty() {
                    "no version"
                } else {
                    &version
                };
                let reason =
                    format!("Codex app-server reported {shown}, which is not a verified version");
                Ok(Self::fall_back(launch, on_spawn, &reason))
            }
            Err(AppServerError::Spawn { .. } | AppServerError::ExitedEarly { .. })
                if !launch.binary_verified =>
            {
                let reason = "the Codex app-server is not available".to_owned();
                Ok(Self::fall_back(launch, on_spawn, &reason))
            }
            Err(other) => Err(other.into()),
        }
    }

    fn fall_back(launch: CodexLaunch, on_spawn: Option<SpawnHook>, reason: &str) -> Self {
        warn!(step = "fall_back_to_exec", reason = %reason, "using Codex exec in the read-only sandbox");
        let mut exec = CodexExecSession::new(CodexSpec {
            binary: launch.binary,
            cwd: launch.cwd,
            session_id: launch.session_id.clone(),
            resume: launch.resume,
            sandbox: SandboxMode::ReadOnly,
            parent_env: launch.parent_env,
        });
        if let Some(hook) = on_spawn {
            exec = exec.with_spawn_hook(hook);
        }
        Self {
            active: Active::Exec(Box::new(exec)),
            session_id: launch.session_id,
            notice: Some(format!(
                "{reason}; approvals are unavailable, so Codex runs read-only"
            )),
            not_carried: false,
        }
    }

    /// The backend that was selected.
    #[must_use]
    pub fn kind(&self) -> BackendKind {
        match self.active {
            Active::AppServer(_) => BackendKind::CodexAppServer,
            Active::Exec(_) => BackendKind::CodexExec,
        }
    }

    /// The running child's process id; the exec backend has none between turns.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        match &self.active {
            Active::AppServer(server) => server.pid(),
            Active::Exec(exec) => exec.pid(),
        }
    }

    /// A handle that cancels this session from another task.
    #[must_use]
    pub fn cancel_handle(&self) -> CancelHandle {
        match &self.active {
            Active::AppServer(server) => server.cancel_handle(),
            Active::Exec(exec) => exec.cancel_handle(),
        }
    }

    /// The thread id a later exec session can resume, once known.
    #[must_use]
    pub fn session_id_from_backend(&self) -> Option<&str> {
        match &self.active {
            Active::AppServer(_) => None,
            Active::Exec(exec) => exec.session_id_from_backend(),
        }
    }

    /// Runs one prompt to the end of the turn, passing every event to `emit`.
    ///
    /// # Errors
    /// Returns the error of the selected backend.
    pub async fn run_turn<G: PermissionGate>(
        &mut self,
        turn_id: &TurnId,
        text: &str,
        gate: &G,
        emit: &mut impl FnMut(Event),
    ) -> Result<TurnOutcome, CodexSessionError> {
        if let Some(reason) = self.notice.take() {
            emit(Event::ApprovalsUnavailable {
                session_id: self.session_id.clone(),
                reason,
            });
        }
        if self.not_carried {
            emit(Event::StreamDelta {
                session_id: self.session_id.clone(),
                turn_id: turn_id.clone(),
                text: NOT_CARRIED_NOTICE.to_owned(),
            });
        }
        match &mut self.active {
            Active::AppServer(server) => Ok(server.run_turn(turn_id, text, gate, emit).await?),
            Active::Exec(exec) => Ok(exec.run_turn(turn_id, text, emit).await?),
        }
    }

    /// Stops the whole process group and refuses further turns; safe to repeat.
    ///
    /// # Errors
    /// Returns the error of the selected backend.
    pub async fn shutdown(&mut self) -> Result<(), CodexSessionError> {
        match &mut self.active {
            Active::AppServer(server) => Ok(server.shutdown().await?),
            Active::Exec(exec) => Ok(exec.shutdown().await?),
        }
    }
}
