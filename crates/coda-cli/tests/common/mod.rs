// SPDX-License-Identifier: AGPL-3.0-only

#![allow(dead_code)]

use std::io::Cursor;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::Duration;

use coda_cli::{Context, Io, Wait, run};
use coda_client::{Client, Subscription};
use coda_config::config::Dirs;
use coda_config::timing::Timing;
use coda_daemon::server::{Server, ServerConfig};
use coda_daemon::state::DaemonState;
use coda_daemon::subscribe::{DEFAULT_QUEUE_CAPACITY, Hub};
use coda_store::Store;
use serde_json::Value;
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
        Self::serve(base, true)
    }

    /// A daemon without a store, which answers store-backed methods with
    /// "method not found" like a daemon built before they existed.
    pub fn start_without_store(base: &Path) -> Self {
        Self::serve(base, false)
    }

    fn serve(base: &Path, with_store: bool) -> Self {
        let dirs = Dirs::from_base(base);
        let uid = std::fs::metadata(base).unwrap().uid();
        let state = DaemonState::new(dirs.data_dir());
        let mut bound = Server::bind(ServerConfig::new(dirs.socket_path(), uid)).unwrap();
        if with_store {
            let store = Store::open(&dirs.data_dir()).unwrap();
            bound = bound.with_hub(Hub::new(store, DEFAULT_QUEUE_CAPACITY));
        }
        let bound = bound.with_state(Arc::clone(&state));
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

pub const POLL: Duration = Duration::from_millis(50);

/// Kills the daemon a test started and the backend groups it left in `run/`,
/// even when an assertion fails first.
pub struct Daemon {
    pub child: Child,
    pub run_dir: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let Ok(entries) = std::fs::read_dir(&self.run_dir) else {
            return;
        };
        for path in entries.filter_map(|entry| Some(entry.ok()?.path())) {
            if path.extension().is_none_or(|ext| ext != "pid") {
                continue;
            }
            let pid = std::fs::read(&path)
                .ok()
                .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
                .and_then(|record| record["pid"].as_u64());
            if let Some(pid) = pid {
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{pid}")])
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

pub fn start_daemon(home: &Path) -> Daemon {
    let fake = coda_testkit::paths::fake_claude_path().unwrap();
    let config = format!(
        "[providers.claude_code]\nenabled = true\nbinary = {:?}\n",
        fake.display().to_string()
    );
    std::fs::write(home.join("config.toml"), config).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_coda"))
        .arg("daemon")
        .env("CODA_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("coda binary should start");
    Daemon {
        child,
        run_dir: Dirs::from_base(home).run_dir(),
    }
}

pub fn timing() -> Timing {
    Timing {
        idle_timeout: WAIT,
        ..Timing::for_tests()
    }
}

pub async fn connect(home: &Path) -> Client {
    let socket = Dirs::from_base(home).socket_path();
    tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(client) = Client::connect(&socket, timing()).await {
                return client;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the daemon did not accept connections in time")
}

/// Reads notifications until `matches` accepts one and returns its params.
pub async fn next_matching(
    subscription: &mut Subscription,
    matches: impl Fn(&str, &Value) -> bool,
) -> Value {
    tokio::time::timeout(WAIT, async {
        while let Some(notification) = subscription.next().await {
            let params = notification.params.unwrap_or(Value::Null);
            if matches(&notification.method, &params) {
                return params;
            }
        }
        panic!("the subscription ended first");
    })
    .await
    .expect("the expected notification did not arrive in time")
}
