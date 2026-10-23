// SPDX-License-Identifier: AGPL-3.0-only

#![allow(dead_code)]

use std::io::{BufRead, Cursor, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// An upper bound for waits, not a duration: starting a process can stall for
/// many seconds on a loaded machine, and a wait returns as soon as its event does.
pub const WAIT: Duration = Duration::from_secs(60);

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

/// The file the faux provider logs every request to, inside the home.
pub const REQUEST_LOG: &str = "requests.jsonl";

/// A home whose faux provider plays a script and logs the requests it gets, and
/// a work directory that is a repository.
pub struct FauxRig {
    // First, so the daemon is stopped while its home directory still exists.
    pub _stop: StopDaemon,
    pub home: TempDir,
    pub work: TempDir,
    pub ctx: Context,
}

/// A [`FauxRig`] that plays `script`.
pub fn faux_rig(script: &[Value]) -> FauxRig {
    let home = short_dir();
    let work = short_dir();
    std::fs::create_dir(work.path().join(".git")).unwrap();
    std::fs::write(
        home.path().join("script.json"),
        Value::Array(script.to_vec()).to_string(),
    )
    .unwrap();
    let config = format!(
        "[providers.faux]\nenabled = true\nscript = {:?}\nrequest_log = {:?}\n",
        home.path().join("script.json").display().to_string(),
        home.path().join(REQUEST_LOG).display().to_string(),
    );
    std::fs::write(home.path().join("config.toml"), config).unwrap();
    let ctx = context(home.path());
    FauxRig {
        _stop: StopDaemon::new(home.path()),
        home,
        work,
        ctx,
    }
}

impl FauxRig {
    /// Every request the faux provider received, oldest first.
    pub fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.home.path().join(REQUEST_LOG))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

pub struct Outcome {
    pub code: ExitCode,
    pub out: String,
    pub err: String,
}

/// Runs the CLI in-process with `input` as the answer to any prompt.
pub async fn cli(ctx: &Context, args: &[&str], input: &str) -> Outcome {
    let mut input = Cursor::new(input.as_bytes().to_vec());
    cli_reading(ctx, args, &mut input).await
}

/// Like [`cli`], with the answers read from `input`, so a test can watch
/// whether the command reads them.
pub async fn cli_reading(ctx: &Context, args: &[&str], input: &mut dyn BufRead) -> Outcome {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = {
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            input,
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
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const PID_PREFIX: &str = "pid: ";

/// Stops the daemon that a test started through the real binary, and waits for
/// it to exit, even when an assertion fails first.
///
/// The daemon is found through `coda status` in its home directory, so declare
/// this guard after the temporary directory that holds the home: locals drop in
/// reverse declaration order (struct fields in declaration order), and the guard
/// must run while that directory still exists.
pub struct StopDaemon(PathBuf);

impl StopDaemon {
    pub fn new(home: &Path) -> Self {
        Self(home.to_path_buf())
    }
}

impl Drop for StopDaemon {
    fn drop(&mut self) {
        let mut status = Command::new(env!("CARGO_BIN_EXE_coda"));
        status.arg("status").env("CODA_HOME", &self.0);
        let Some(out) = output_within(&mut status, STOP_TIMEOUT) else {
            return;
        };
        let Some(pid) = out.lines().find_map(|line| line.strip_prefix(PID_PREFIX)) else {
            return;
        };
        signal("-TERM", pid);
        let deadline = Instant::now() + STOP_TIMEOUT;
        while signal("-0", pid) {
            if Instant::now() >= deadline {
                signal("-KILL", pid);
                return;
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Runs `command` and returns its standard output, or `None` if it outlasts `limit`.
fn output_within(command: &mut Command, limit: Duration) -> Option<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + limit;
    while child.try_wait().ok()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(POLL);
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

fn signal(name: &str, pid: &str) -> bool {
    Command::new("kill")
        .args([name, pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

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
