// SPDX-License-Identifier: AGPL-3.0-only

#![allow(dead_code)]

use std::io::Cursor;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use coda_cli::{Context, Io, Wait, run};
use coda_config::config::Dirs;
use coda_daemon::server::{Server, ServerConfig};
use coda_daemon::state::DaemonState;
use coda_daemon::subscribe::{DEFAULT_QUEUE_CAPACITY, Hub};
use coda_store::Store;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub const WAIT: Duration = Duration::from_secs(10);

/// macOS limits socket paths to 104 bytes, so keep them under /tmp.
pub fn short_dir() -> TempDir {
    let dir = tempfile::Builder::new()
        .prefix("cc")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A context rooted at `base` that never spawns the real binary unless asked to.
pub fn context(base: &Path) -> Context {
    let mut ctx = Context::new(
        Dirs::from_base(base),
        PathBuf::from(env!("CARGO_BIN_EXE_coda")),
        base.to_path_buf(),
    );
    ctx.wait = Wait {
        timeout: WAIT,
        poll: Duration::from_millis(20),
    };
    ctx
}

pub struct Outcome {
    pub code: ExitCode,
    pub out: String,
    pub err: String,
}

/// Runs the CLI in-process with `input` as the answer to any prompt.
pub async fn cli(ctx: &Context, args: &[&str], input: &str) -> Outcome {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut input = Cursor::new(input.as_bytes().to_vec());
    let code = {
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            input: &mut input,
        };
        tokio::time::timeout(WAIT, run(args.iter().copied(), ctx, &mut io))
            .await
            .expect("the command timed out")
    };
    Outcome {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

/// A daemon serving in this process on the socket `Dirs` names.
pub struct TestDaemon {
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<()>,
}

impl TestDaemon {
    pub fn start(base: &Path) -> Self {
        let dirs = Dirs::from_base(base);
        let uid = std::fs::metadata(base).unwrap().uid();
        let store = Store::open(&dirs.data_dir()).unwrap();
        let hub = Hub::new(store, DEFAULT_QUEUE_CAPACITY);
        let state = DaemonState::new(dirs.data_dir());
        let bound = Server::bind(ServerConfig::new(dirs.socket_path(), uid))
            .unwrap()
            .with_hub(hub)
            .with_state(Arc::clone(&state));
        let (shutdown, stop) = oneshot::channel();
        let handle = tokio::spawn(bound.serve(async move {
            tokio::select! {
                _ = stop => {}
                () = state.shutdown_requested() => {}
            }
        }));
        Self { shutdown, handle }
    }

    pub async fn stop(self) {
        let _ = self.shutdown.send(());
        tokio::time::timeout(WAIT, self.handle)
            .await
            .expect("the daemon did not stop")
            .unwrap();
    }
}
