// SPDX-License-Identifier: AGPL-3.0-only

//! Facts about the running daemon that its control requests report or change.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use coda_protocol::methods::{DaemonStatusResult, PROTOCOL_VERSION};
use tokio::sync::Notify;

/// Process-wide daemon state shared by the dispatcher and the server.
#[derive(Debug)]
pub struct DaemonState {
    started: Instant,
    data_dir: PathBuf,
    clients: AtomicU32,
    shutdown: Notify,
}

/// Counts one open connection until dropped.
#[derive(Debug)]
pub struct ClientGuard {
    state: Arc<DaemonState>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.state.clients.fetch_sub(1, Ordering::Relaxed);
    }
}

impl DaemonState {
    /// State for a daemon that stores its data under `data_dir`, started now.
    #[must_use]
    pub fn new(data_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            started: Instant::now(),
            data_dir,
            clients: AtomicU32::new(0),
            shutdown: Notify::new(),
        })
    }

    /// Registers an open connection; it stays counted while the guard lives.
    #[must_use]
    pub fn track_client(self: &Arc<Self>) -> ClientGuard {
        self.clients.fetch_add(1, Ordering::Relaxed);
        ClientGuard {
            state: Arc::clone(self),
        }
    }

    /// Snapshot answered to `daemon.status`.
    #[must_use]
    pub fn status(&self) -> DaemonStatusResult {
        DaemonStatusResult {
            pid: std::process::id(),
            protocol_version: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            uptime_secs: self.started.elapsed().as_secs(),
            client_count: self.clients.load(Ordering::Relaxed),
            // No turn loop exists yet, so nothing can be running.
            active_turns: 0,
            data_dir: self.data_dir.display().to_string(),
        }
    }

    /// Asks the server to stop. Remembered if nobody waits yet.
    pub fn request_shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// Completes once [`Self::request_shutdown`] was called.
    pub async fn shutdown_requested(&self) {
        self.shutdown.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use coda_protocol::methods::PROTOCOL_VERSION;

    use super::DaemonState;

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn status_reports_process_and_protocol_facts() {
        // Arrange
        let state = DaemonState::new(PathBuf::from("/data"));

        // Act
        let status = state.status();

        // Assert
        assert_eq!(status.pid, std::process::id());
        assert_eq!(status.protocol_version, PROTOCOL_VERSION);
        assert_eq!(status.data_dir, "/data");
        assert_eq!(status.active_turns, 0);
    }

    #[test]
    fn tracked_clients_are_counted_until_dropped() {
        // Arrange
        let state = DaemonState::new(PathBuf::from("/data"));
        let first = state.track_client();
        let second = state.track_client();

        // Act
        let while_open = state.status().client_count;
        drop(first);
        drop(second);
        let after_close = state.status().client_count;

        // Assert
        assert_eq!(while_open, 2);
        assert_eq!(after_close, 0);
    }

    #[tokio::test]
    async fn shutdown_request_wakes_a_waiter_even_when_made_first() {
        // Arrange
        let state = DaemonState::new(PathBuf::from("/data"));
        state.request_shutdown();

        // Act
        let woke = tokio::time::timeout(WAIT, state.shutdown_requested()).await;

        // Assert
        assert!(woke.is_ok());
    }
}
