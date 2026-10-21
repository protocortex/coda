// SPDX-License-Identifier: AGPL-3.0-only

//! `/model` and `/backend` against a real daemon with scripted model sessions.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Arc;
use std::time::Duration;

use coda_config::config::{Config, ModelAlias, ProviderConfig};
use coda_config::timing::Timing;
use coda_daemon::server::{Server, ServerConfig};
use coda_daemon::subscribe::Hub;
use coda_daemon::turn::{TurnConfig, Turns};
use coda_daemon::watchdog;
use coda_protocol::types::{BackendKind, SessionInfo};
use coda_store::Store;
use coda_tui::app::LinkEvent;
use coda_tui::sessions::{LinkCommand, LinkConfig, TRANSCRIPT_NOT_CARRIED, run_link};
use serde_json::{Value, json};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

/// Upper bounds, not durations: they only matter when the machine is badly loaded.
const WAIT: Duration = Duration::from_secs(60);
/// `Timing::for_tests` lets a daemon request take 50 ms, which a busy machine misses.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const QUEUE: usize = 64;

/// Synthetic script: a tool call that waits for a permission answer.
fn blocking_script() -> Value {
    json!([{"events": [
        {"type": "tool_call_start", "call_id": "c1", "name": "bash"},
        {"type": "tool_call_arguments_delta", "call_id": "c1", "fragment": "{\"command\":\"true\"}"},
        {"type": "usage", "input_tokens": 5, "output_tokens": 3},
        {"type": "stop", "reason": "tool_use"},
    ]}])
}

struct Rig {
    dir: tempfile::TempDir,
    _data: tempfile::TempDir,
    _repo: tempfile::TempDir,
    cwd: String,
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
        let repo = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(repo.path()).unwrap();
        std::fs::create_dir(cwd.join(".git")).unwrap();
        let script = data.path().join("script.json");
        std::fs::write(&script, blocking_script().to_string()).unwrap();
        let uid = std::fs::metadata(dir.path()).unwrap().uid();
        let hub = Hub::new(Store::open(&data.path().join("store")).unwrap(), QUEUE);
        let turns = Turns::new(
            Arc::clone(&hub),
            TurnConfig {
                binary: None,
                pidfile_dir: data.path().to_path_buf(),
                parent_env: vec![("PATH".into(), "/usr/bin:/bin".into())],
                timing: watchdog::Timing::default(),
                models: config(script),
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
            _repo: repo,
            cwd: cwd.to_string_lossy().into_owned(),
            _stop: stop,
        }
    }

    fn link(&self) -> Link {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        tokio::spawn(run_link(
            self.dir.path().join("s.sock"),
            Timing {
                idle_timeout: REQUEST_TIMEOUT,
                ..Timing::for_tests()
            },
            LinkConfig {
                backend: BackendKind::Faux,
                cwd: self.cwd.clone(),
            },
            command_rx,
            event_tx,
        ));
        Link { commands, events }
    }
}

/// Faux and Anthropic are enabled, `OpenAI`-compatible is disabled, and a
/// Claude Code section is absent, so the daemon lists exactly two backends.
fn config(script: std::path::PathBuf) -> Config {
    let mut config = Config::default();
    config.providers.insert(
        "faux".to_owned(),
        ProviderConfig {
            enabled: true,
            script: Some(script),
            ..ProviderConfig::default()
        },
    );
    for (key, enabled) in [("anthropic", true), ("openai_compat", false)] {
        config.providers.insert(
            key.to_owned(),
            ProviderConfig {
                enabled,
                api_key_env: Some("NO_SUCH_KEY_VARIABLE".to_owned()),
                base_url: Some("http://localhost:1/v1".to_owned()),
                ..ProviderConfig::default()
            },
        );
    }
    config.models.insert(
        "scripted".to_owned(),
        ModelAlias {
            provider: "faux".to_owned(),
            model: "script-1".to_owned(),
            context_window: None,
        },
    );
    config
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

    async fn notice(&mut self) -> String {
        self.wait_for(|event| match event {
            LinkEvent::Notice(text) => Some(text),
            _ => None,
        })
        .await
    }

    async fn notice_containing(&mut self, needle: &str) -> String {
        self.wait_for(|event| match event {
            LinkEvent::Notice(text) if text.contains(needle) => Some(text),
            _ => None,
        })
        .await
    }

    fn send(&self, command: LinkCommand) {
        self.commands.send(command).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn model_switches_the_session_to_the_alias_model() {
    // Arrange
    let rig = Rig::start();
    let mut link = rig.link();
    let session = link.started().await;

    // Act
    link.send(LinkCommand::SetModel("scripted".to_owned()));
    let switched = link
        .wait_for(|event| match event {
            LinkEvent::ModelSwitched(info) => Some(info),
            _ => None,
        })
        .await;

    // Assert
    assert_eq!(switched.id, session.id);
    assert_eq!(switched.model.as_deref(), Some("script-1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn model_with_an_unknown_alias_shows_the_daemon_message() {
    // Arrange
    let rig = Rig::start();
    let mut link = rig.link();
    link.started().await;

    // Act
    link.send(LinkCommand::SetModel("nope".to_owned()));
    let notice = link.notice().await;

    // Assert
    assert!(notice.contains("could not switch the model"), "{notice}");
    assert!(notice.contains("unknown model alias `nope`"), "{notice}");
}

#[tokio::test(flavor = "multi_thread")]
async fn model_is_refused_while_a_turn_is_running() {
    // Arrange
    let rig = Rig::start();
    let mut link = rig.link();
    link.started().await;
    link.send(LinkCommand::Send("run the tool".to_owned()));
    link.wait_for(|event| matches!(event, LinkEvent::Permission(_)).then_some(()))
        .await;

    // Act
    link.send(LinkCommand::SetModel("scripted".to_owned()));
    let notice = link.notice_containing("could not switch the model").await;

    // Assert
    assert!(notice.contains("A turn is already running"), "{notice}");
}

#[tokio::test(flavor = "multi_thread")]
async fn backend_lists_the_enabled_backends_from_the_daemon() {
    // Arrange
    let rig = Rig::start();
    let mut link = rig.link();
    link.started().await;

    // Act
    link.send(LinkCommand::ListBackends);
    let notice = link.notice_containing("backends").await;

    // Assert
    assert!(notice.contains("anthropic"), "{notice}");
    assert!(notice.contains("faux"), "{notice}");
    assert!(!notice.contains("openai_compat"), "{notice}");
    assert!(!notice.contains("claude_code"), "{notice}");
}

#[tokio::test(flavor = "multi_thread")]
async fn backend_starts_a_new_session_and_says_the_transcript_is_not_carried_over() {
    // Arrange
    let rig = Rig::start();
    let mut link = rig.link();
    let first = link.started().await;

    // Act
    link.send(LinkCommand::SwitchBackend("anthropic".to_owned()));
    let second = link.started().await;
    let notice = link.notice().await;

    // Assert
    assert_ne!(second.id, first.id);
    assert_eq!(second.backend, BackendKind::Anthropic);
    assert_eq!(notice, TRANSCRIPT_NOT_CARRIED);
}

#[tokio::test(flavor = "multi_thread")]
async fn backend_rejects_an_unknown_and_a_disabled_backend() {
    // Arrange
    let rig = Rig::start();
    let mut link = rig.link();
    link.started().await;

    for name in ["nope", "openai_compat"] {
        // Act
        link.send(LinkCommand::SwitchBackend(name.to_owned()));
        let notice = link.notice().await;

        // Assert
        assert!(
            notice.contains(&format!("`{name}` is not available")),
            "{notice}"
        );
        assert!(notice.contains("anthropic, faux"), "{notice}");
    }
}
