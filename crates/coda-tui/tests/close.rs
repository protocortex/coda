// SPDX-License-Identifier: AGPL-3.0-only

//! Leaving a session the UI created stops its pooled Codex server.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Arc;
use std::time::Duration;

use coda_client::Client;
use coda_config::config::Config;
use coda_config::timing::Timing;
use coda_daemon::server::{Server, ServerConfig};
use coda_daemon::subscribe::Hub;
use coda_daemon::turn::{TurnConfig, Turns};
use coda_daemon::watchdog;
use coda_protocol::events::Event;
use coda_protocol::methods::{SESSION_CANCEL, SessionCancelParams, SessionCancelResult};
use coda_protocol::types::{BackendKind, SessionInfo};
use coda_store::Store;
use coda_tui::app::LinkEvent;
use coda_tui::sessions::{LinkCommand, LinkConfig, run_link};
use serde_json::{Value, json};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Upper bounds, not durations: they only matter when the machine is badly loaded.
const WAIT: Duration = Duration::from_secs(60);
/// `Timing::for_tests` lets a daemon request take 50 ms, which a busy machine misses.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(20);
const QUEUE: usize = 64;
const REPORT: &str = "report.json";

struct Rig {
    dir: tempfile::TempDir,
    _data: tempfile::TempDir,
    work: tempfile::TempDir,
    _stop: oneshot::Sender<()>,
}

impl Rig {
    fn start() -> Self {
        // macOS limits socket paths to 104 bytes, so keep them under /tmp.
        let dir = tempfile::Builder::new()
            .prefix("ct")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let work = tempfile::Builder::new()
            .prefix("cw")
            .tempdir_in("/tmp")
            .unwrap();
        let control = json!({"report_path": work.path().join(REPORT)});
        std::fs::write(
            work.path().join("fake-codex-control.json"),
            control.to_string(),
        )
        .unwrap();
        let uid = std::fs::metadata(dir.path()).unwrap().uid();
        let hub = Hub::new(Store::open(&data.path().join("store")).unwrap(), QUEUE);
        let turns = Turns::new(
            Arc::clone(&hub),
            TurnConfig {
                binary: None,
                pidfile_dir: data.path().to_path_buf(),
                parent_env: vec![("PATH".into(), "/usr/bin:/bin".into())],
                timing: watchdog::Timing::default(),
                models: codex_config(),
            },
        );
        let bound = Server::bind(ServerConfig::new(dir.path().join("s.sock"), uid))
            .unwrap()
            .with_hub(hub)
            .with_turns(turns);
        let (stop, rx) = oneshot::channel();
        tokio::spawn(bound.serve(async {
            let _ = rx.await;
        }));
        Self {
            dir,
            _data: data,
            work,
            _stop: stop,
        }
    }

    fn link(&self) -> (Link, JoinHandle<()>) {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_link(
            self.dir.path().join("s.sock"),
            Timing {
                idle_timeout: REQUEST_TIMEOUT,
                ..Timing::for_tests()
            },
            LinkConfig {
                backend: BackendKind::CodexAppServer,
                cwd: self.work.path().to_string_lossy().into_owned(),
            },
            command_rx,
            event_tx,
        ));
        (Link { commands, events }, task)
    }

    /// Polls until the fake app-server has received a prompt.
    async fn wait_for_prompt(&self) {
        tokio::time::timeout(WAIT, async {
            loop {
                let seen = std::fs::read_to_string(self.work.path().join(REPORT))
                    .ok()
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                    .is_some_and(|report| {
                        report["prompts"].as_array().is_some_and(|p| !p.is_empty())
                    });
                if seen {
                    return;
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("the server did not receive the prompt in time");
    }

    /// The process id the fake app-server reported for the server running now.
    fn server_pid(&self) -> rustix::process::Pid {
        let raw = std::fs::read_to_string(self.work.path().join(REPORT)).unwrap();
        let report: Value = serde_json::from_str(&raw).unwrap();
        let pid = i32::try_from(report["pid"].as_i64().unwrap()).unwrap();
        rustix::process::Pid::from_raw(pid).unwrap()
    }
}

fn request_timing() -> Timing {
    Timing {
        idle_timeout: REQUEST_TIMEOUT,
        ..Timing::for_tests()
    }
}

fn codex_config() -> Config {
    let binary = coda_testkit::paths::fake_codex_path().unwrap();
    let toml = format!(
        "[providers.codex]\nenabled = true\nbinary = \"{}\"\nverified_versions = [\"0.0.0-fake\"]\n",
        binary.display()
    );
    Config::from_toml_str(&toml).unwrap().config
}

struct Link {
    commands: UnboundedSender<LinkCommand>,
    events: UnboundedReceiver<LinkEvent>,
}

impl Link {
    async fn wait_for<T>(&mut self, pick: impl Fn(LinkEvent) -> Option<T>) -> T {
        tokio::time::timeout(WAIT, async {
            while let Some(event) = self.events.recv().await {
                if let Some(found) = pick(event) {
                    return found;
                }
            }
            panic!("the link ended");
        })
        .await
        .expect("timed out waiting for a link event")
    }

    async fn started(&mut self) -> SessionInfo {
        self.wait_for(|event| match event {
            LinkEvent::SessionStarted(info) => Some(info),
            _ => None,
        })
        .await
    }

    /// Sends `text` and waits for the turn's result.
    async fn turn(&mut self, text: &str) {
        self.commands
            .send(LinkCommand::Send(text.to_owned()))
            .unwrap();
        self.wait_for(|event| {
            matches!(event, LinkEvent::Session(Event::Result { .. })).then_some(())
        })
        .await;
    }
}

async fn assert_gone(pid: rustix::process::Pid) {
    tokio::time::timeout(WAIT, async {
        while rustix::process::test_kill_process(pid).is_ok() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the server process is still alive");
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_away_from_a_session_the_ui_created_stops_its_server() {
    // Arrange
    let rig = Rig::start();
    let (mut link, _task) = rig.link();
    link.started().await;
    link.turn("hello").await;
    let pid = rig.server_pid();
    assert!(rustix::process::test_kill_process(pid).is_ok());

    // Act
    link.commands.send(LinkCommand::New).unwrap();
    link.started().await;

    // Assert
    assert_gone(pid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_the_link_stops_the_server_of_a_session_the_ui_created() {
    // Arrange
    let rig = Rig::start();
    let (mut link, task) = rig.link();
    link.started().await;
    link.turn("hello").await;
    let pid = rig.server_pid();

    // Act
    link.commands.send(LinkCommand::Close).unwrap();
    tokio::time::timeout(WAIT, task)
        .await
        .expect("the link did not end")
        .unwrap();

    // Assert
    assert_gone(pid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_whose_close_was_refused_is_closed_when_the_link_ends() {
    // Arrange
    let rig = Rig::start();
    let (mut link, task) = rig.link();
    let first = link.started().await;
    link.commands
        .send(LinkCommand::Send("please stall".to_owned()))
        .unwrap();
    rig.wait_for_prompt().await;
    let pid = rig.server_pid();
    link.commands.send(LinkCommand::New).unwrap();
    link.started().await;
    // The refused close left the stalled turn running; end it from another connection.
    let other = Client::connect(&rig.dir.path().join("s.sock"), request_timing())
        .await
        .unwrap();
    let cancelled: SessionCancelResult = other
        .request(
            SESSION_CANCEL,
            &SessionCancelParams {
                session_id: first.id,
            },
        )
        .await
        .unwrap();

    // Act
    link.commands.send(LinkCommand::Close).unwrap();
    tokio::time::timeout(WAIT, task)
        .await
        .expect("the link did not end")
        .unwrap();

    // Assert
    assert!(cancelled.cancelled);
    assert_gone(pid).await;
}
