// SPDX-License-Identifier: AGPL-3.0-only

//! Command files as slash commands: offered, listed, expanded and sent.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use coda_agents::{Catalog, trust_project};
use coda_config::config::{Config, ProviderConfig};
use coda_config::timing::Timing;
use coda_daemon::server::{Server, ServerConfig};
use coda_daemon::subscribe::Hub;
use coda_daemon::turn::{TurnConfig, Turns};
use coda_daemon::watchdog;
use coda_protocol::events::Event;
use coda_protocol::types::BackendKind;
use coda_store::Store;
use coda_tui::app::{App, LinkEvent};
use coda_tui::commands::UserCommands;
use coda_tui::sessions::{LinkCommand, LinkConfig, run_link};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::oneshot;

/// An upper bound, not a duration: it only matters when the machine is badly loaded.
const WAIT: Duration = Duration::from_secs(60);
/// `Timing::for_tests` lets a daemon request take 50 ms, which a busy machine misses.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const QUEUE: usize = 64;
const SCREEN_WIDTH: u16 = 160;
const SCREEN_HEIGHT: u16 = 24;
const REQUEST_LOG: &str = "requests.jsonl";

const REVIEW: &str = "+++\nname = \"review\"\ndescription = \"Reviews a change.\"\n\
    argument_hint = \"<change>\"\n+++\nReview $ARGUMENTS and report problems only.\n";
const STATUS: &str = "+++\nname = \"status\"\ndescription = \"Summarizes the repo.\"\n+++\n\
    Summarize the repository.\n";
const SHADOW: &str =
    "+++\nname = \"new\"\ndescription = \"Starts something.\"\n+++\nDo $ARGUMENTS\n";
const DEPLOY: &str =
    "+++\nname = \"deploy\"\ndescription = \"Ships it.\"\n+++\nDeploy $ARGUMENTS\n";

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

struct Files {
    user: tempfile::TempDir,
    repo: tempfile::TempDir,
    data: tempfile::TempDir,
}

fn files() -> Files {
    Files {
        user: tempfile::tempdir().unwrap(),
        repo: tempfile::tempdir().unwrap(),
        data: tempfile::tempdir().unwrap(),
    }
}

impl Files {
    fn user_command(&self, name: &str, text: &str) {
        write(self.user.path(), &format!("commands/{name}.md"), text);
    }

    fn project_command(&self, name: &str, text: &str) {
        write(self.repo.path(), &format!(".coda/commands/{name}.md"), text);
    }

    /// The commands as they are with the project files not trusted.
    fn untrusted(&self) -> UserCommands {
        UserCommands::from_catalog(&Catalog::load(self.user.path(), Some(self.repo.path())))
    }

    /// The commands once the user trusted the project files as they are now.
    fn trusted(&self) -> UserCommands {
        let mut store = Store::open(&self.data.path().join("data")).unwrap();
        trust_project(&mut store, self.repo.path()).unwrap();
        let catalog =
            Catalog::load_trusted(&store, self.user.path(), Some(self.repo.path())).unwrap();
        UserCommands::from_catalog(&catalog)
    }
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn type_line(app: &mut App, line: &str) {
    app.handle_paste(line);
    app.handle_key(press(KeyCode::Enter));
}

fn app_with(commands: UserCommands) -> (App, UnboundedReceiver<LinkCommand>) {
    let (link, sent) = mpsc::unbounded_channel();
    let mut app = App::with_link(link);
    app.set_user_commands(commands);
    (app, sent)
}

fn sent(commands: &mut UnboundedReceiver<LinkCommand>) -> Vec<LinkCommand> {
    std::iter::from_fn(|| commands.try_recv().ok()).collect()
}

fn screen(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(SCREEN_WIDTH, SCREEN_HEIGHT)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(SCREEN_WIDTH))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_user_command_file_runs_as_a_slash_command_and_sends_its_expanded_prompt() {
    // Arrange
    let f = files();
    f.user_command("review", REVIEW);
    let (mut app, mut link) = app_with(f.untrusted());

    // Act
    type_line(&mut app, "/review the parser change");

    // Assert
    assert_eq!(
        sent(&mut link),
        [LinkCommand::Send(
            "Review the parser change and report problems only.".to_owned()
        )]
    );
}

#[test]
fn text_that_looks_like_a_placeholder_in_the_arguments_is_sent_as_typed() {
    // Arrange
    let f = files();
    f.user_command("review", REVIEW);
    let (mut app, mut link) = app_with(f.untrusted());

    // Act
    type_line(&mut app, "/review $ARGUMENTS ${HOME} $(id)");

    // Assert
    assert_eq!(
        sent(&mut link),
        [LinkCommand::Send(
            "Review $ARGUMENTS ${HOME} $(id) and report problems only.".to_owned()
        )]
    );
}

#[test]
fn arguments_for_a_command_without_a_placeholder_send_nothing_and_say_why() {
    // Arrange
    let f = files();
    f.user_command("status", STATUS);
    let (mut app, mut link) = app_with(f.untrusted());

    // Act
    type_line(&mut app, "/status now please");

    // Assert
    assert_eq!(sent(&mut link), []);
    let shown = screen(&app);
    assert!(shown.contains("takes no arguments"), "{shown}");
}

#[test]
fn a_built_in_wins_a_name_clash_and_the_clash_is_reported() {
    // Arrange
    let f = files();
    f.user_command("new", SHADOW);
    let (mut app, mut link) = app_with(f.untrusted());

    // Act
    type_line(&mut app, "/new");

    // Assert
    assert_eq!(sent(&mut link), [LinkCommand::New]);
    let shown = screen(&app);
    assert!(
        shown.contains("/new is a built-in command"),
        "the clash was not reported: {shown}"
    );
}

#[test]
fn a_pending_project_command_is_listed_as_pending_and_does_not_run() {
    // Arrange
    let f = files();
    f.project_command("deploy", DEPLOY);
    let (mut app, mut link) = app_with(f.untrusted());

    // Act
    type_line(&mut app, "/deploy now");
    type_line(&mut app, "/help");

    // Assert
    assert_eq!(sent(&mut link), []);
    let shown = screen(&app);
    assert!(
        shown.contains("/deploy is a project command that is not in use"),
        "{shown}"
    );
    assert!(
        shown.contains("/deploy: project command, pending"),
        "{shown}"
    );
    assert!(shown.contains("coda agents trust"), "{shown}");
}

#[test]
fn a_trusted_project_command_runs_and_help_names_its_source() {
    // Arrange
    let f = files();
    f.project_command("deploy", DEPLOY);
    let (mut app, mut link) = app_with(f.trusted());

    // Act
    type_line(&mut app, "/deploy staging");
    type_line(&mut app, "/help");

    // Assert
    assert_eq!(
        sent(&mut link),
        [LinkCommand::Send("Deploy staging".to_owned())]
    );
    let shown = screen(&app);
    assert!(shown.contains("/deploy: Ships it. (project)"), "{shown}");
    assert!(!shown.contains("pending"), "{shown}");
}

#[test]
fn help_lists_the_built_ins_and_each_command_file_with_its_hint() {
    // Arrange
    let f = files();
    f.user_command("review", REVIEW);
    f.user_command("status", STATUS);
    let (mut app, mut link) = app_with(f.untrusted());

    // Act
    type_line(&mut app, "/help");

    // Assert
    assert_eq!(sent(&mut link), []);
    let shown = screen(&app);
    assert!(shown.contains("Built-in commands: /sessions"), "{shown}");
    assert!(
        shown.contains("/review <change>: Reviews a change. (user)"),
        "{shown}"
    );
    assert!(
        shown.contains("/status: Summarizes the repo. (user)"),
        "{shown}"
    );
}

#[test]
fn an_unknown_command_is_still_an_error_and_never_reaches_the_model() {
    // Arrange
    let f = files();
    f.user_command("review", REVIEW);
    let (mut app, mut link) = app_with(f.untrusted());

    // Act
    type_line(&mut app, "/reveiw the change");

    // Assert
    assert_eq!(sent(&mut link), []);
    let shown = screen(&app);
    assert!(shown.contains("unknown command /reveiw"), "{shown}");
}

/// Control bytes and an escape sequence, as an untrusted file could carry them.
const HOSTILE: &str = "\\u0007\\u001b[31mred\\u001b[0m";

#[test]
fn help_draws_no_control_byte_from_a_description_a_hint_or_a_pending_file_name() {
    // Arrange
    let f = files();
    let hostile_command = format!(
        "+++\nname = \"loud\"\ndescription = \"Loud {HOSTILE} text.\"\n\
         argument_hint = \"<x{HOSTILE}>\"\n+++\nGo $ARGUMENTS\n"
    );
    f.user_command("loud", &hostile_command);
    // The name of a file that waits for trust is not parsed, so it is raw text.
    f.project_command("bad\u{1b}[31mname\u{7}", DEPLOY);
    let commands = f.untrusted();

    // Act
    let lines = commands.help_lines();

    // Assert
    let text = lines.join("\n");
    assert!(text.contains("Loud"), "{text}");
    assert!(text.contains("[31mname"), "{text}");
    for line in &lines {
        assert!(
            !line.chars().any(char::is_control),
            "a help line holds a raw control byte: {line:?}"
        );
    }
}

#[test]
fn a_name_that_only_looks_like_a_command_file_is_refused_before_any_model_call() {
    for line in [
        "/Deploy now",
        "/deploy/",
        "/deploy/ now",
        "/deploy//",
        "/DEPLOY",
    ] {
        // Arrange
        let f = files();
        f.user_command("deploy", DEPLOY);
        let (mut app, mut link) = app_with(f.untrusted());

        // Act
        type_line(&mut app, line);

        // Assert
        assert_eq!(sent(&mut link), [], "{line}");
        let shown = screen(&app);
        assert!(shown.contains("unknown command /"), "{line}: {shown}");
    }
}

#[test]
fn a_path_is_still_a_message_even_when_it_starts_with_a_command_name() {
    for line in ["/var/log/deploy x", "/deploy/staging x", "/deploy/a.log x"] {
        // Arrange
        let f = files();
        f.user_command("deploy", DEPLOY);
        let (mut app, mut link) = app_with(f.untrusted());

        // Act
        type_line(&mut app, line);

        // Assert
        assert_eq!(sent(&mut link), [LinkCommand::Send(line.to_owned())]);
    }
}

#[test]
fn a_pending_project_command_named_after_a_built_in_is_reported_as_a_clash() {
    // Arrange
    let f = files();
    f.project_command("new", SHADOW);
    let (mut app, mut link) = app_with(f.untrusted());

    // Act
    type_line(&mut app, "/help");
    type_line(&mut app, "/new");

    // Assert
    assert_eq!(sent(&mut link), [LinkCommand::New]);
    let shown = screen(&app);
    assert!(shown.contains("/new is a built-in command"), "{shown}");
    assert!(shown.contains("project command file /new"), "{shown}");
    assert!(!shown.contains("/new: project command, pending"), "{shown}");
}

#[test]
fn a_command_file_failure_is_a_warning_and_the_built_ins_still_work() {
    // Arrange
    let (mut app, mut link) = app_with(UserCommands::unavailable(
        "the command files could not be read: the store is locked",
    ));

    // Act
    type_line(&mut app, "/new");
    type_line(&mut app, "/help");

    // Assert
    assert_eq!(sent(&mut link), [LinkCommand::New]);
    let shown = screen(&app);
    assert!(shown.contains("the store is locked"), "{shown}");
}

/// A daemon serving the faux provider, which logs what the model is sent.
struct Rig {
    dir: tempfile::TempDir,
    data: tempfile::TempDir,
    cwd: String,
    _repo: tempfile::TempDir,
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
        let reply = json!([{"events": [
            {"type": "text_delta", "text": "done"},
            {"type": "usage", "input_tokens": 5, "output_tokens": 3},
            {"type": "stop", "reason": "end_turn"},
        ]}]);
        std::fs::write(&script, reply.to_string()).unwrap();
        let mut models = Config::default();
        models.providers.insert(
            "faux".to_owned(),
            ProviderConfig {
                enabled: true,
                script: Some(script),
                request_log: Some(data.path().join(REQUEST_LOG)),
                ..ProviderConfig::default()
            },
        );
        let uid = std::fs::metadata(dir.path()).unwrap().uid();
        let hub = Hub::new(Store::open(&data.path().join("store")).unwrap(), QUEUE);
        let turns = Turns::new(
            Arc::clone(&hub),
            TurnConfig {
                binary: None,
                pidfile_dir: data.path().to_path_buf(),
                parent_env: vec![("PATH".into(), "/usr/bin:/bin".into())],
                timing: watchdog::Timing::default(),
                models,
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
            data,
            cwd: cwd.to_string_lossy().into_owned(),
            _repo: repo,
            _stop: stop,
        }
    }

    fn logged_requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.data.path().join(REQUEST_LOG))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

/// Feeds the link's events to `app` until `done` accepts one.
async fn pump(
    app: &mut App,
    events: &mut UnboundedReceiver<LinkEvent>,
    done: impl Fn(&LinkEvent) -> bool,
) {
    tokio::time::timeout(WAIT, async {
        while let Some(event) = events.recv().await {
            let finished = done(&event);
            app.handle_event(event);
            if finished {
                return;
            }
        }
        panic!("the link ended");
    })
    .await
    .expect("timed out waiting for a link event");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_file_typed_in_the_ui_reaches_the_model_as_its_expanded_prompt() {
    // Arrange
    let rig = Rig::start();
    let f = files();
    f.user_command("review", REVIEW);
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (event_tx, mut events) = mpsc::unbounded_channel();
    tokio::spawn(run_link(
        rig.dir.path().join("s.sock"),
        Timing {
            idle_timeout: REQUEST_TIMEOUT,
            ..Timing::for_tests()
        },
        LinkConfig {
            backend: BackendKind::Faux,
            cwd: rig.cwd.clone(),
        },
        command_rx,
        event_tx,
    ));
    let mut app = App::with_link(commands);
    app.set_user_commands(f.untrusted());
    pump(&mut app, &mut events, |e| {
        matches!(e, LinkEvent::SessionStarted(_))
    })
    .await;

    // Act
    type_line(&mut app, "/review PR 42");
    pump(&mut app, &mut events, |e| {
        matches!(e, LinkEvent::Session(Event::Result { .. }))
    })
    .await;

    // Assert
    let expected = "Review PR 42 and report problems only.";
    let shown = screen(&app);
    assert!(shown.contains(&format!("you: {expected}")), "{shown}");
    let requests = rig.logged_requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0]["messages"][0],
        json!({"role": "user", "text": expected})
    );
}
