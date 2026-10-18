// SPDX-License-Identifier: AGPL-3.0-only

//! Cleanup of what a daemon that died without cleaning up leaves in its run
//! directory, for `coda doctor --repair`.

use std::path::{Path, PathBuf};

use tracing::info;

use crate::lock::{InstanceLock, LockError};
use crate::reaper::{ReapReport, reap_orphans};
use crate::server::{ServerError, ensure_private_dir, lock_path_for, remove_stale_socket};

/// What a repair removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepairReport {
    /// A socket file nobody listened on was removed.
    pub socket_removed: bool,
    /// Something that is not a socket sits at the socket path and was left alone.
    pub socket_path_blocked: bool,
    /// Orphaned backends killed and stale pidfiles removed.
    pub reaped: ReapReport,
}

/// Why a repair did not run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RepairError {
    #[error("a daemon holds the instance lock at {}, so nothing was touched", path.display())]
    DaemonRunning { path: PathBuf },
    #[error(transparent)]
    Lock(LockError),
    #[error(transparent)]
    Socket(#[from] ServerError),
}

/// Removes a stale socket and kills orphaned backends. It takes the instance
/// lock first, so it refuses to touch anything while a daemon is alive, and it
/// leaves the lock file in place: unlinking it would let a new daemon lock a
/// different inode than one that still holds the old file. Running it again
/// finds nothing left to do.
///
/// # Errors
/// Returns [`RepairError::DaemonRunning`] when the lock is held,
/// [`RepairError::Socket`] when the run directory is not private or something
/// answers on the socket, and [`RepairError::Lock`] when the lock file cannot
/// be used.
pub async fn repair(socket_path: &Path, pidfile_dir: &Path) -> Result<RepairReport, RepairError> {
    if !pidfile_dir.is_dir() {
        return Ok(RepairReport::default());
    }
    let uid = rustix::process::getuid().as_raw();
    ensure_private_dir(pidfile_dir, uid)?;
    info!(step = "take_lock", "taking the instance lock");
    let _lock = InstanceLock::acquire(&lock_path_for(socket_path)).map_err(|e| match e {
        LockError::AlreadyRunning { path } => RepairError::DaemonRunning { path },
        other => RepairError::Lock(other),
    })?;
    info!(step = "remove_socket", "removing a stale socket");
    let socket_existed = socket_path.symlink_metadata().is_ok();
    // A non-socket at the path does not stop the backends from being reaped.
    let socket_path_blocked = match remove_stale_socket(socket_path, uid) {
        Ok(()) => false,
        Err(ServerError::SocketPathNotASocket { .. }) => true,
        Err(e) => return Err(e.into()),
    };
    info!(step = "reap_backends", "reaping orphaned backends");
    let reaped = reap_orphans(pidfile_dir).await;
    Ok(RepairReport {
        socket_removed: socket_existed && socket_path.symlink_metadata().is_err(),
        socket_path_blocked,
        reaped,
    })
}
