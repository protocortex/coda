// SPDX-License-Identifier: AGPL-3.0-only

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use coda_config::timing::Timing;
use tempfile::TempDir;

pub const WAIT: Duration = Duration::from_secs(5);

/// A request timeout generous enough for loaded machines.
pub fn timing() -> Timing {
    Timing {
        idle_timeout: WAIT,
        ..Timing::for_tests()
    }
}

/// macOS limits socket paths to 104 bytes, so keep them under /tmp.
pub fn short_dir() -> TempDir {
    let dir = tempfile::Builder::new()
        .prefix("cc")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// Bounds a future so a hang fails the test instead of stalling it.
pub async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future)
        .await
        .expect("timed out waiting")
}
