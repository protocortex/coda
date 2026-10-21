// SPDX-License-Identifier: AGPL-3.0-only

//! Single-instance guard: an exclusive advisory lock on a file held for the daemon lifetime.

use std::fs::{File, OpenOptions, Permissions, TryLockError};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const LOCK_FILE_MODE: u32 = 0o600;

/// Failures taking the instance lock.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LockError {
    #[error("another daemon already holds the lock at {}", path.display())]
    AlreadyRunning { path: PathBuf },
    #[error("could not use the lock file {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
}

/// An exclusive lock released when dropped (or when the process exits).
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Creates the lock file with mode 0600 if needed and locks it without blocking.
    ///
    /// # Errors
    /// Returns [`LockError::AlreadyRunning`] if another holder has the lock, or
    /// [`LockError::Io`] if the file cannot be opened or locked.
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
        let io_error = |source| LockError::Io {
            path: path.to_path_buf(),
            source,
        };
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(LOCK_FILE_MODE)
            .open(path)
            .map_err(io_error)?;
        // `mode` only applies on creation, so tighten a pre-existing file too.
        file.set_permissions(Permissions::from_mode(LOCK_FILE_MODE))
            .map_err(io_error)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(LockError::AlreadyRunning {
                path: path.to_path_buf(),
            }),
            Err(TryLockError::Error(source)) => Err(io_error(source)),
        }
    }
}

/// Retries `attempt` while a lock or socket this test just released is still held. A child forked by a
/// concurrent test inherits every open descriptor until it execs, so a released
/// lock or closed listener can stay live for a few milliseconds.
#[cfg(test)]
pub(crate) fn retry_while_held<T>(
    mut attempt: impl FnMut() -> Result<T, crate::server::ServerError>,
) -> Result<T, crate::server::ServerError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match attempt() {
            Err(
                crate::server::ServerError::Lock(LockError::AlreadyRunning { .. })
                | crate::server::ServerError::SocketInUse { .. },
            ) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn acquire_creates_lock_file_with_mode_0600() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");

        // Act
        let _lock = InstanceLock::acquire(&path).unwrap();

        // Assert
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn second_acquire_reports_already_running() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        let _first = InstanceLock::acquire(&path).unwrap();

        // Act
        let second = InstanceLock::acquire(&path);

        // Assert
        assert!(matches!(second, Err(LockError::AlreadyRunning { .. })));
    }

    #[test]
    fn dropping_the_lock_lets_another_instance_start() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        drop(InstanceLock::acquire(&path).unwrap());

        // Act
        let again = retry_while_held(|| Ok(InstanceLock::acquire(&path)?));

        // Assert
        assert!(again.is_ok());
    }

    #[test]
    fn acquire_tightens_a_looser_existing_lock_file() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // Act
        let _lock = InstanceLock::acquire(&path).unwrap();

        // Assert
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
