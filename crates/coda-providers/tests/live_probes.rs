// SPDX-License-Identifier: Apache-2.0

//! Opt-in probes of the installed `claude`, run after every upgrade of it. They
//! check the facts the adapter and the context profiles rest on: how big the
//! prompt of each profile is, what the init message lists, that usage and rate
//! limit reports are read, which control requests answer, and what a message
//! sent in the middle of a turn does.
//!
//! They use the login of the user who runs them, the unmodified `claude`, the
//! same cleared environment a session gets, and a cheap model with a turn limit.
//! They spend a few short calls of the user's own plan, so every test is
//! `#[ignore]` and also needs `CODA_LIVE_TESTS=1`; CI never runs them.
//!
//! ```sh
//! CODA_LIVE_TESTS=1 cargo nextest run -p coda-providers --test live_probes \
//!   --run-ignored only --test-threads 1
//! ```
//!
//! Nothing here writes a fixture and no test prints a token or an account id.
//! See `docs/live-recording.md`.
#![cfg(unix)]

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use coda_config::config::ClaudeProfile;
use coda_protocol::events::{AccountBilling, Event};
use coda_protocol::types::{PermissionDecision, SessionId, TurnId};
use coda_providers::account::{AccountStatus, billing_of_api_key_source};
use coda_providers::claude_helper::{DEFAULT_TIMEOUT, HelperRequest, run_structured};
use coda_providers::claude_session::{ClaudeSession, ENV_ALLOWLIST, SessionSpec, TurnOutcome};
use coda_providers::codex_app_server::{AppServerSpec, read_account};
use coda_providers::codex_exec::SandboxMode;
use coda_providers::gate::PermissionGate;
use coda_providers::init_check::InitReport;
use coda_providers::tool_request::ToolRequest;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

const LIVE_SWITCH: &str = "CODA_LIVE_TESTS";
const LIVE_ON: &str = "1";
const TURN_BOUND: Duration = Duration::from_secs(120);
const LINE_BOUND: Duration = Duration::from_secs(60);
const CHEAP_MODEL: &str = "haiku";
const MAX_TURNS: &str = "3";
/// Prompt tokens a profile may send before the first word, measured with an
/// empty working directory so no project file adds to it. Measured with claude
/// 2.1.296 on 2026-11-08: `project` 22.3k and `lean` 5.2k. The ceilings leave about
/// 10 percent for `project` and over a factor of two for `lean`, so a new version
/// that grows the prompt much more than that fails the probe.
const PROJECT_PROMPT_CEILING: u64 = 25_000;
const LEAN_PROMPT_CEILING: u64 = 12_000;
const TEXT_PROMPT: &str = "Reply with the single word: ok";
const LONG_PROMPT: &str = "Write the numbers 1 to 150, one per line, and nothing else.";
const SECOND_PROMPT: &str = "Now reply with only the word steered.";
const HELPER_PROMPT: &str =
    "Make a title of at most six words for a session about adding a retry to an http client.";
const RESUME_AT_FLAG: &str = "--resume-session-at";
const FORK_FLAG: &str = "--fork-session";
const CONTROL_SUCCESS: &str = "success";
const BASE_ARGS: &[&str] = &[
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--permission-prompt-tool",
    "stdio",
];
const CONTAINMENT_ARGS: &[&str] = &[
    "--setting-sources",
    "project",
    "--strict-mcp-config",
    "--disable-slash-commands",
];

fn live() -> bool {
    if std::env::var(LIVE_SWITCH).as_deref() == Ok(LIVE_ON) {
        return true;
    }
    eprintln!("skipped: set {LIVE_SWITCH}={LIVE_ON} to probe the installed claude");
    false
}

fn find_claude() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("claude"))
        .find(|candidate| candidate.is_file())
        .expect("`claude` is not on PATH")
}

/// The variables a session gets: the allowlist and nothing else.
fn session_env() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(name, _)| ENV_ALLOWLIST.iter().any(|allowed| name == allowed))
        .collect()
}

struct DenyAll;

impl PermissionGate for DenyAll {
    fn request(
        &self,
        _request: &ToolRequest,
    ) -> impl std::future::Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::Deny)
    }
}

/// A shell shim that adds the cheap model and a turn limit and copies the
/// stream `claude` writes to a file, so the init message can be read back.
fn shim(dir: &Path, real: &Path) -> (PathBuf, PathBuf) {
    let received = dir.join("received.jsonl");
    let script = format!(
        "#!/bin/sh\n'{}' \"$@\" --model {CHEAP_MODEL} --max-turns {MAX_TURNS} | /usr/bin/tee -a '{}'\n",
        real.to_str().expect("UTF-8 path"),
        received.to_str().expect("UTF-8 path"),
    );
    let path = dir.join("claude-shim.sh");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    (path, received)
}

struct ProfileRun {
    events: Vec<Event>,
    stream: Vec<Value>,
}

async fn run_profile(profile: ClaudeProfile) -> ProfileRun {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (binary, received) = shim(dir.path(), &find_claude());
    let mut session = ClaudeSession::spawn(SessionSpec {
        binary,
        cwd: cwd.path().to_path_buf(),
        session_id: SessionId::new("probe"),
        resume: None,
        parent_env: session_env(),
        env_passthrough: Vec::new(),
        read_only: false,
        profile,
    })
    .expect("could not start claude");
    let mut events = Vec::new();
    let outcome = timeout(
        TURN_BOUND,
        session.run_turn(&TurnId::new("t-1"), TEXT_PROMPT, &DenyAll, &mut |event| {
            events.push(event);
        }),
    )
    .await
    .expect("the turn did not finish in time")
    .expect("the turn failed");
    session.shutdown().await.unwrap();
    assert_eq!(
        outcome,
        TurnOutcome::Completed { is_error: false },
        "{profile:?}"
    );
    let stream = fs::read_to_string(received)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    ProfileRun { events, stream }
}

fn prompt_tokens(run: &ProfileRun) -> u64 {
    run.events
        .iter()
        .find_map(|event| match event {
            Event::Usage { usage, .. } => {
                Some(usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens)
            }
            _ => None,
        })
        .expect("no usage event was read")
}

fn init_of(run: &ProfileRun) -> &Value {
    run.stream
        .iter()
        .find(|line| line["type"] == "system" && line["subtype"] == "init")
        .expect("the stream has no init message")
}

fn check_profile(run: &ProfileRun, profile: ClaudeProfile) -> u64 {
    let tokens = prompt_tokens(run);
    let unexpected = InitReport::from_init(init_of(run)).unexpected(profile);
    assert_eq!(unexpected, Vec::<String>::new(), "{profile:?} init");
    let source = init_of(run)["apiKeySource"].as_str().unwrap_or("none");
    assert_eq!(
        billing_of_api_key_source(source),
        AccountBilling::Subscription,
        "{profile:?}: the probe would bill API rates"
    );
    let limit_lines = run
        .stream
        .iter()
        .filter(|line| line["type"] == "rate_limit_event")
        .count();
    let limit_events = run
        .events
        .iter()
        .filter(|event| matches!(event, Event::RateLimit { .. }))
        .count();
    assert!(limit_lines > 0, "{profile:?}: no rate_limit_event was sent");
    assert_eq!(
        limit_events, limit_lines,
        "{profile:?}: every rate_limit_event line becomes an event"
    );
    eprintln!("{profile:?}: {tokens} prompt tokens, {limit_lines} rate limit reports");
    tokens
}

#[ignore = "spends Claude subscription quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn each_profile_stays_under_its_prompt_ceiling_and_loads_only_what_it_should() {
    if !live() {
        return;
    }
    let full = check_profile(&run_profile(ClaudeProfile::Full).await, ClaudeProfile::Full);
    let project = check_profile(
        &run_profile(ClaudeProfile::Project).await,
        ClaudeProfile::Project,
    );
    let lean = check_profile(&run_profile(ClaudeProfile::Lean).await, ClaudeProfile::Lean);

    assert!(project <= PROJECT_PROMPT_CEILING, "project sent {project}");
    assert!(lean <= LEAN_PROMPT_CEILING, "lean sent {lean}");
    assert!(
        lean < project && project < full,
        "full {full}, project {project}, lean {lean}"
    );
}

/// A `claude` child driven by hand, to send control requests the adapter does not.
struct RawClaude {
    /// Held so the process is killed when the probe ends.
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Lines read while looking for a control answer, handed out first.
    backlog: VecDeque<Value>,
}

impl RawClaude {
    /// Starts in `cwd`, which a probe that resumes a session must share.
    fn start(cwd: &Path, extra: &[&str]) -> Self {
        let mut command = Command::new(find_claude());
        command
            .args(BASE_ARGS)
            .args(CONTAINMENT_ARGS)
            .args(["--model", CHEAP_MODEL, "--max-turns", MAX_TURNS])
            .args(extra)
            .env_clear()
            .envs(session_env())
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("could not start claude");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            _child: child,
            stdin,
            stdout,
            backlog: VecDeque::new(),
        }
    }

    async fn send(&mut self, value: &Value) {
        let mut line = value.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn user(&mut self, text: &str) {
        self.send(&json!({"type": "user", "message": {"role": "user", "content": text}}))
            .await;
    }

    async fn control(&mut self, id: &str, request: Value) {
        self.send(&json!({"type": "control_request", "request_id": id, "request": request}))
            .await;
    }

    /// Ends the input, so the child finishes what it has and exits.
    async fn close_input(&mut self) {
        self.stdin.shutdown().await.unwrap();
    }

    /// The next JSON line, or `None` at the end of the stream or after `bound`.
    async fn next(&mut self, bound: Duration) -> Option<Value> {
        if let Some(line) = self.backlog.pop_front() {
            return Some(line);
        }
        self.read(bound).await
    }

    async fn read(&mut self, bound: Duration) -> Option<Value> {
        loop {
            let mut line = String::new();
            let read = timeout(bound, self.stdout.read_line(&mut line))
                .await
                .ok()?;
            if read.ok()? == 0 {
                return None;
            }
            if let Ok(value) = serde_json::from_str(&line) {
                return Some(value);
            }
        }
    }

    /// The turn's `result` line, whether it came before or after the answer
    /// to a control request.
    async fn next_result(&mut self) -> Value {
        self.until(is_result).await
    }

    /// Reads until a line satisfies `found`, within `LINE_BOUND` for each line.
    async fn until(&mut self, found: impl Fn(&Value) -> bool) -> Value {
        while let Some(line) = self.next(LINE_BOUND).await {
            if found(&line) {
                return line;
            }
        }
        panic!("the stream ended before the line it was read for");
    }

    /// The answer to request `id`. Other lines read on the way stay for later, so
    /// two answers may come in either order.
    async fn control_answer(&mut self, id: &str) -> Value {
        let is_answer = |line: &Value| {
            line["type"] == "control_response" && line["response"]["request_id"] == id
        };
        if let Some(at) = self.backlog.iter().position(is_answer) {
            return self.backlog.remove(at).unwrap()["response"].clone();
        }
        while let Some(line) = self.read(LINE_BOUND).await {
            if is_answer(&line) {
                return line["response"].clone();
            }
            self.backlog.push_back(line);
        }
        panic!("the stream ended before the answer to {id}");
    }
}

fn is_init(line: &Value) -> bool {
    line["type"] == "system" && line["subtype"] == "init"
}

fn is_result(line: &Value) -> bool {
    line["type"] == "result"
}

fn keys(value: &Value) -> Vec<String> {
    value
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default()
}

#[ignore = "spends Claude subscription quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn the_initialize_request_answers_with_an_account_block() {
    if !live() {
        return;
    }
    let cwd = tempfile::tempdir().unwrap();
    let mut claude = RawClaude::start(cwd.path(), &[]);
    claude
        .control("p-init", json!({"subtype": "initialize"}))
        .await;

    let answer = claude.control_answer("p-init").await;

    assert_eq!(
        answer["subtype"],
        CONTROL_SUCCESS,
        "{}",
        keys(&answer).join(",")
    );
    let account = &answer["response"]["account"];
    // Only the names of the fields are printed, never their values.
    eprintln!("initialize account fields: {}", keys(account).join(", "));
    assert!(account.is_object(), "no account block");
}

#[ignore = "spends Claude subscription quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn set_permission_mode_and_set_model_are_answered_without_a_turn() {
    if !live() {
        return;
    }
    let cwd = tempfile::tempdir().unwrap();
    let mut claude = RawClaude::start(cwd.path(), &[]);
    claude
        .control(
            "p-mode",
            json!({"subtype": "set_permission_mode", "mode": "plan"}),
        )
        .await;
    claude
        .control(
            "p-model",
            json!({"subtype": "set_model", "model": CHEAP_MODEL}),
        )
        .await;

    let mode = claude.control_answer("p-mode").await;
    let model = claude.control_answer("p-model").await;

    assert_eq!(mode["subtype"], CONTROL_SUCCESS, "{mode}");
    assert_eq!(model["subtype"], CONTROL_SUCCESS, "{model}");
}

#[ignore = "spends Claude subscription quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn the_interrupt_request_stops_a_running_turn() {
    if !live() {
        return;
    }
    let cwd = tempfile::tempdir().unwrap();
    let mut claude = RawClaude::start(cwd.path(), &[]);
    claude.user(LONG_PROMPT).await;
    claude.until(|line| line["type"] == "stream_event").await;
    claude
        .control("p-int", json!({"subtype": "interrupt"}))
        .await;

    let answer = claude.control_answer("p-int").await;

    assert_eq!(answer["subtype"], CONTROL_SUCCESS, "{answer}");
    let result = claude.next_result().await;
    eprintln!(
        "result after interrupt: subtype {}, is_error {}",
        result["subtype"], result["is_error"]
    );
    assert_eq!(result["is_error"], true, "the turn was not interrupted");
}

#[ignore = "spends Claude subscription quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn a_message_sent_in_the_middle_of_a_turn_is_steered_or_queued_not_refused() {
    if !live() {
        return;
    }
    let cwd = tempfile::tempdir().unwrap();
    let mut claude = RawClaude::start(cwd.path(), &[]);
    claude.user(LONG_PROMPT).await;
    claude.until(|line| line["type"] == "stream_event").await;
    claude.user(SECOND_PROMPT).await;
    claude.close_input().await;

    let mut results = 0_u32;
    while let Some(line) = claude.next(LINE_BOUND).await {
        if is_result(&line) {
            assert_eq!(line["is_error"], false, "{}", line["subtype"]);
            results += 1;
        }
    }

    let finding = match results {
        1 => "steer: one result answered both messages",
        2 => "queue: each message got its own result",
        _ => "other",
    };
    eprintln!("mid-turn message: {finding}");
    assert!(results == 1 || results == 2, "{results} results");
}

#[ignore = "spends Claude subscription quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn fork_session_starts_a_new_session_from_a_resumed_one() {
    if !live() {
        return;
    }
    let cwd = tempfile::tempdir().unwrap();
    let mut first = RawClaude::start(cwd.path(), &[]);
    first.user(TEXT_PROMPT).await;
    let init = first.until(is_init).await;
    let original = init["session_id"].as_str().unwrap().to_owned();
    first.until(is_result).await;
    drop(first);

    let mut forked = RawClaude::start(cwd.path(), &["--resume", &original, FORK_FLAG]);
    forked.user(TEXT_PROMPT).await;
    let init = forked.until(is_init).await;
    let result = forked.until(is_result).await;

    assert_ne!(init["session_id"].as_str().unwrap(), original);
    assert_eq!(result["is_error"], false);
}

#[ignore = "spends no quota; needs CODA_LIVE_TESTS=1"]
#[test]
fn the_cli_has_no_flag_to_resume_at_a_message_but_has_fork_session() {
    if !live() {
        return;
    }
    let help = std::process::Command::new(find_claude())
        .arg("--help")
        .env_clear()
        .envs(session_env())
        .stdin(Stdio::null())
        .output()
        .expect("could not run claude --help");
    let text = String::from_utf8_lossy(&help.stdout);

    assert!(text.contains(FORK_FLAG), "{FORK_FLAG} is gone");
    assert!(
        !text.contains(RESUME_AT_FLAG),
        "{RESUME_AT_FLAG} exists now: coda's rewind can use it"
    );
}

#[ignore = "spends Claude subscription quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn the_helper_call_returns_json_that_fits_its_schema() {
    if !live() {
        return;
    }
    let schema = json!({
        "type": "object",
        "properties": {"title": {"type": "string"}},
        "required": ["title"],
    });
    let env = session_env();
    let binary = find_claude();

    let answer = run_structured(&HelperRequest {
        binary: &binary,
        parent_env: &env,
        env_passthrough: &[],
        prompt: HELPER_PROMPT,
        schema: &schema,
        model: Some(CHEAP_MODEL),
        timeout: DEFAULT_TIMEOUT,
    })
    .await
    .expect("the helper call failed");

    let title = answer["title"].as_str().expect("no title");
    assert_ne!(title.trim(), "");
    eprintln!("helper title length: {} characters", title.len());
}

fn find_codex() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("codex"))
        .find(|candidate| candidate.is_file())
        .expect("`codex` is not on PATH")
}

fn help_of(binary: &Path, args: &[&str]) {
    let help = std::process::Command::new(binary)
        .args(args)
        .arg("--help")
        .stdin(Stdio::null())
        .output()
        .expect("could not run the help command");
    assert!(help.status.success(), "`--help` failed for {args:?}");
}

/// `coda login` runs `claude auth login` and `codex login`, and `coda doctor` and
/// the UI start-up offer ask `claude auth status` and the Codex app-server. This
/// checks that those commands still exist and that the probes read the user's own
/// login. It does not run a login: the browser step needs the user, so it stays a
/// manual check, listed in `docs/live-recording.md`.
#[ignore = "spends no quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn the_login_commands_exist_and_the_probes_read_the_installed_logins() {
    if !live() {
        return;
    }
    let claude = find_claude();
    let codex = find_codex();

    help_of(&claude, &["auth", "login"]);
    help_of(&codex, &["login"]);

    let claude_status = coda_providers::account::probe_claude(
        &claude,
        &std::env::vars_os().collect::<Vec<_>>(),
        &[],
        LINE_BOUND,
    )
    .expect("the claude probe could not read `auth status`");
    assert!(
        matches!(claude_status, AccountStatus::LoggedIn(_)),
        "claude is not logged in for this probe"
    );

    let spec = AppServerSpec {
        binary: codex,
        cwd: std::env::current_dir().unwrap(),
        session_id: SessionId::new("live-login"),
        sandbox: SandboxMode::default(),
        ephemeral: true,
        verified_versions: Vec::new(),
        parent_env: std::env::vars_os().collect(),
        resume_thread: None,
    };
    let codex_status = timeout(LINE_BOUND, read_account(&spec))
        .await
        .expect("the codex probe timed out")
        .expect("the codex probe could not read the account");
    assert!(
        matches!(codex_status, AccountStatus::LoggedIn(_)),
        "codex is not logged in for this probe"
    );
}
