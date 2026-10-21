// SPDX-License-Identifier: AGPL-3.0-only

//! `coda run`: sends one prompt to a new session, follows it to the end and
//! exits with a code that names why the turn stopped.
//!
//! The run is headless: nobody can answer a permission prompt, so every
//! prompt is denied and the model carries on without that tool call.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use coda_client::{Client, ClientError, Subscription};
use coda_config::config::Config;
use coda_core::turn::StopCondition;
use coda_daemon::model_session::TURN_STOPPED_KIND;
use coda_daemon::subscribe::PERMISSION_REQUEST_NOTIFICATION;
use coda_protocol::events::EVENT_NOTIFICATION;
use coda_protocol::methods::{
    PERMISSION_RESPOND, PermissionRequestParams, PermissionRespondParams, PermissionRespondResult,
    SESSION_CANCEL, SESSION_CREATE, SESSION_SEND, SessionCancelParams, SessionCancelResult,
    SessionCreateParams, SessionCreateResult, SessionSendParams, SessionSendResult,
};
use coda_protocol::types::{BackendKind, PermissionDecision, SessionId};
use serde::Deserialize;
use serde_json::Value;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tracing::{debug, info, warn};

use crate::autostart::{connect_unchecked, ensure_daemon};
use crate::cli::RunArgs;
use crate::context::{Context, Io};
use crate::daemon_cmd::load_config;
use crate::error::CliError;
use crate::exit;

const FAUX_KEY: &str = "faux";
const CLAUDE_CODE_KEY: &str = "claude_code";
const CODEX_KEY: &str = "codex";
const STREAM_DELTA_KIND: &str = "stream_delta";
const RESULT_KIND: &str = "result";
const TURN_INTERRUPTED_KIND: &str = "turn_interrupted";

/// How long an interrupted run waits for the daemon to answer the send, and
/// then, separately, for it to end the turn. A second Ctrl-C ends either wait.
const CANCEL_GRACE: Duration = Duration::from_secs(30);

const END_TURN: &str = StopCondition::EndTurn.as_str();
const MAX_TURNS: &str = StopCondition::MaxTurns.as_str();
const BUDGET_EXHAUSTED: &str = StopCondition::BudgetExhausted.as_str();
const CONTEXT_OVERFLOW: &str = StopCondition::ContextOverflow.as_str();
const COMPACTION_FAILED: &str = StopCondition::CompactionFailed.as_str();
const CANCELLED: &str = StopCondition::Cancelled.as_str();

/// The fields of a `session.event` notification this command reads.
#[derive(Deserialize)]
struct EventParams {
    kind: String,
    body: String,
}

/// How a followed turn ended.
struct Finish {
    condition: Option<String>,
    failed: bool,
    /// The daemon cut the turn short instead of reporting a result.
    cut_short: bool,
}

/// Runs the prompt, from `--prompt` or else standard input, in a new session
/// and returns the exit code for how it stopped.
///
/// # Errors
/// Returns [`CliError::EmptyPrompt`] when there is nothing to send,
/// [`CliError::RunInterrupted`] when the daemon connection is lost before the
/// turn ends, or the errors of reaching the daemon and of its requests.
///
/// The first Ctrl-C cancels the turn and the run reports how it ended; a second
/// one quits at once with [`CliError::RunInterrupted`]. Without a second one, an
/// interrupted run waits at most [`CANCEL_GRACE`] for the daemon to answer the
/// send, plus the timeouts of its cancel requests, and then [`CANCEL_GRACE`]
/// again for the turn to end.
pub async fn run(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
    args: RunArgs,
) -> Result<ExitCode, CliError> {
    info!(step = "read_prompt", "reading the prompt");
    let prompt = read_prompt(io, args.prompt)?;
    info!(step = "connect", "reaching the daemon");
    let client = ensure_daemon(ctx, io, force).await?;
    info!(step = "create_session", "creating the session");
    let created: SessionCreateResult = client
        .request(
            SESSION_CREATE,
            &SessionCreateParams {
                backend: backend(ctx)?,
                cwd: session_cwd(&ctx.cwd, args.cwd)?,
                model: None,
                mode: args.mode.into(),
            },
        )
        .await?;
    let session = created.session.id;
    let mut events = client.subscribe(&session, Some(0)).await?;
    // Registered before the send so an interrupt during it is never lost, and
    // not earlier so Ctrl-C still stops the run while the daemon starts.
    let mut interrupts = signal(SignalKind::interrupt())?;
    info!(step = "send", session = %session, "sending the prompt");
    let params = SessionSendParams {
        session_id: session.clone(),
        text: prompt,
        max_turns: args.max_turns,
        token_cap: args.token_cap,
    };
    let send = client.request::<_, SessionSendResult>(SESSION_SEND, &params);
    tokio::pin!(send);
    let mut interrupted = false;
    tokio::select! {
        answer = &mut send => {
            answer?;
        }
        _ = interrupts.recv() => {
            interrupted = true;
            cancel_during_send(
                &client,
                &session,
                connect_unchecked(ctx),
                send,
                &mut interrupts,
            )
            .await?;
        }
    }
    info!(step = "follow", session = %session, "following the turn");
    let follow = follow(
        &client,
        &mut events,
        &session,
        &mut interrupts,
        interrupted,
        io,
    );
    let finish = if interrupted {
        tokio::time::timeout(CANCEL_GRACE, follow)
            .await
            .map_err(|_| CliError::RunInterrupted)??
    } else {
        follow.await?
    };
    Ok(report(&finish, io)?)
}

/// Cancels a turn whose send the daemon has not answered yet, then waits for
/// that answer. The daemon serves one connection's requests in order, so the
/// first cancel goes over a new connection. It can land before the daemon has
/// reserved the session, or the new connection can fail, so unless it was
/// accepted the cancel is repeated over `client` once the send is answered.
///
/// A second Ctrl-C ends the wait at once with [`CliError::RunInterrupted`];
/// otherwise the send is waited for at most [`CANCEL_GRACE`].
async fn cancel_during_send(
    client: &Client,
    session: &SessionId,
    connect_side: impl Future<Output = Result<Client, CliError>>,
    send: impl Future<Output = Result<SessionSendResult, ClientError>>,
    interrupts: &mut Signal,
) -> Result<(), CliError> {
    info!(session = %session, "interrupted while sending, cancelling the turn");
    let cancel = async {
        let accepted = match connect_side.await {
            Ok(side) => cancel_turn(&side, session).await,
            Err(failure) => {
                warn!(session = %session, error = %failure, "could not reach the daemon over a second connection to cancel");
                false
            }
        };
        tokio::time::timeout(CANCEL_GRACE, send)
            .await
            .map_err(|_| CliError::RunInterrupted)??;
        if !accepted {
            cancel_turn(client, session).await;
        }
        Ok(())
    };
    tokio::select! {
        done = cancel => done,
        _ = interrupts.recv() => Err(CliError::RunInterrupted),
    }
}

/// The prompt flag, or else all of the input.
fn read_prompt(io: &mut Io<'_>, flag: Option<String>) -> Result<String, CliError> {
    let prompt = if let Some(prompt) = flag {
        prompt
    } else {
        let mut text = String::new();
        io.input.read_to_string(&mut text)?;
        text
    };
    if prompt.trim().is_empty() {
        return Err(CliError::EmptyPrompt);
    }
    Ok(prompt)
}

/// The scripted provider when the config enables it, else the delegated
/// Claude Code agent, else the delegated Codex agent, else the Anthropic API.
fn backend(ctx: &Context) -> Result<BackendKind, CliError> {
    Ok(pick_backend(&load_config(&ctx.dirs)?))
}

fn pick_backend(config: &Config) -> BackendKind {
    let enabled = |key| config.providers.get(key).is_some_and(|p| p.enabled);
    if enabled(FAUX_KEY) {
        BackendKind::Faux
    } else if enabled(CLAUDE_CODE_KEY) {
        BackendKind::ClaudeCode
    } else if enabled(CODEX_KEY) {
        BackendKind::CodexAppServer
    } else {
        BackendKind::Anthropic
    }
}

/// Prints streamed text, denies permission prompts and cancels on Ctrl-C,
/// until the turn's result arrives. The first Ctrl-C cancels the turn; a
/// second one, once `interrupted`, gives up waiting for the result and ends the
/// run with [`CliError::RunInterrupted`].
async fn follow(
    client: &Client,
    events: &mut Subscription,
    session: &SessionId,
    interrupts: &mut Signal,
    mut interrupted: bool,
    io: &mut Io<'_>,
) -> Result<Finish, CliError> {
    let mut condition = None;
    let mut printed = false;
    loop {
        let notification = tokio::select! {
            next = events.next() => next.ok_or(CliError::RunInterrupted)?,
            _ = interrupts.recv() => {
                if interrupted {
                    return Err(CliError::RunInterrupted);
                }
                interrupted = true;
                info!(session = %session, "interrupted, cancelling the turn");
                cancel_turn(client, session).await;
                continue;
            }
        };
        let params = notification.params.unwrap_or(Value::Null);
        match notification.method.as_str() {
            PERMISSION_REQUEST_NOTIFICATION => deny(client, params).await?,
            EVENT_NOTIFICATION => {
                let Ok(event) = serde_json::from_value::<EventParams>(params) else {
                    debug!("skipping a malformed event");
                    continue;
                };
                let body: Value = serde_json::from_str(&event.body).unwrap_or(Value::Null);
                match event.kind.as_str() {
                    STREAM_DELTA_KIND => {
                        if let Some(text) = body["text"].as_str() {
                            write!(io.out, "{text}")?;
                            printed = true;
                        }
                    }
                    TURN_STOPPED_KIND => {
                        condition = body["condition"].as_str().map(str::to_owned);
                    }
                    RESULT_KIND => {
                        if printed {
                            writeln!(io.out)?;
                        }
                        let failed = body["is_error"].as_bool().unwrap_or(true);
                        return Ok(Finish {
                            condition,
                            failed,
                            cut_short: false,
                        });
                    }
                    // A delegated backend that is cancelled or goes silent ends
                    // its turn this way, with no result event after it.
                    TURN_INTERRUPTED_KIND => {
                        if printed {
                            writeln!(io.out)?;
                        }
                        return Ok(Finish {
                            condition: interrupted.then(|| CANCELLED.to_owned()),
                            failed: true,
                            cut_short: true,
                        });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Whether the daemon accepted the cancel. It can lose a race with the turn
/// ending; the result still follows, so a failed cancel is only logged.
async fn cancel_turn(client: &Client, session: &SessionId) -> bool {
    let params = SessionCancelParams {
        session_id: session.clone(),
    };
    let cancelled: Result<SessionCancelResult, _> = client.request(SESSION_CANCEL, &params).await;
    match cancelled {
        Ok(result) => result.cancelled,
        Err(failure) => {
            warn!(session = %session, error = %failure, "the cancel request failed; following the turn on");
            false
        }
    }
}

/// The directory the session runs in: the flag, resolved against the
/// invocation directory and canonicalized, else the invocation directory.
fn session_cwd(invoked_in: &Path, flag: Option<PathBuf>) -> Result<String, CliError> {
    let Some(flag) = flag else {
        return Ok(invoked_in.display().to_string());
    };
    let path = invoked_in.join(flag);
    let resolved = path
        .canonicalize()
        .map_err(|source| CliError::Path { path, source })?;
    Ok(resolved.display().to_string())
}

async fn deny(client: &Client, params: Value) -> Result<(), CliError> {
    let Ok(request) = serde_json::from_value::<PermissionRequestParams>(params) else {
        debug!("skipping a malformed permission request");
        return Ok(());
    };
    info!(request = %request.request_id, tool = %request.tool, "denying a permission prompt in a headless run");
    let answer = PermissionRespondParams {
        session_id: request.session_id,
        request_id: request.request_id,
        decision: PermissionDecision::Deny,
    };
    let _: PermissionRespondResult = client.request(PERMISSION_RESPOND, &answer).await?;
    Ok(())
}

fn report(finish: &Finish, io: &mut Io<'_>) -> std::io::Result<ExitCode> {
    // A delegated backend reports a result without a stop condition.
    let code = match (finish.condition.as_deref(), finish.failed) {
        (Some(condition), _) => exit_code(condition),
        (None, false) => exit::RUN_END_TURN,
        (None, true) => exit::RUN_OTHER,
    };
    if code == exit::RUN_OTHER {
        let reason = match (&finish.condition, finish.cut_short) {
            (Some(condition), _) if condition == COMPACTION_FAILED => {
                "the conversation could not be compacted to fit the context window; \
                 see the daemon log"
                    .to_owned()
            }
            (Some(condition), _) => format!("the run stopped: {condition}"),
            (None, true) => "the run was cut short; see the daemon log".to_owned(),
            (None, false) => "the run failed; see the daemon log".to_owned(),
        };
        writeln!(io.err, "coda: {reason}")?;
    }
    Ok(ExitCode::from(code))
}

/// The exit code for a stop condition the daemon reported.
fn exit_code(condition: &str) -> u8 {
    match condition {
        END_TURN => exit::RUN_END_TURN,
        MAX_TURNS => exit::RUN_MAX_TURNS,
        BUDGET_EXHAUSTED => exit::RUN_BUDGET_EXHAUSTED,
        CONTEXT_OVERFLOW => exit::RUN_CONTEXT_OVERFLOW,
        CANCELLED => exit::RUN_CANCELLED,
        _ => exit::RUN_OTHER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(toml: &str) -> BackendKind {
        pick_backend(&Config::from_toml_str(toml).unwrap().config)
    }

    #[test]
    fn the_backend_is_faux_then_claude_code_then_codex_then_anthropic() {
        // Arrange
        let faux = "[providers.faux]\nenabled = true\n";
        let claude = "[providers.claude_code]\nenabled = true\n";
        let codex = "[providers.codex]\nenabled = true\n";

        // Act and Assert
        assert_eq!(pick(&format!("{codex}{claude}{faux}")), BackendKind::Faux);
        assert_eq!(pick(&format!("{codex}{claude}")), BackendKind::ClaudeCode);
        assert_eq!(pick(claude), BackendKind::ClaudeCode);
        assert_eq!(pick(codex), BackendKind::CodexAppServer);
        assert_eq!(
            pick("[providers.claude_code]\nenabled = false\n"),
            BackendKind::Anthropic
        );
        assert_eq!(pick(""), BackendKind::Anthropic);
    }

    #[test]
    fn every_stop_condition_maps_to_its_exit_code() {
        for (condition, code) in [
            ("end_turn", 0),
            ("max_turns", 1),
            ("budget_exhausted", 2),
            ("context_overflow", 3),
            ("cancelled", 4),
            ("refused", 5),
            ("something_new", 5),
        ] {
            // Act and Assert
            assert_eq!(exit_code(condition), code, "{condition}");
        }
    }

    #[test]
    fn the_session_directory_without_a_flag_is_the_invocation_directory() {
        // Act
        let cwd = session_cwd(Path::new("/work/repo"), None).unwrap();

        // Assert
        assert_eq!(cwd, "/work/repo");
    }

    #[test]
    fn a_session_directory_flag_is_resolved_and_canonicalized() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        std::fs::create_dir(base.join("sub")).unwrap();

        // Act
        let cwd = session_cwd(&base, Some(PathBuf::from("sub/."))).unwrap();

        // Assert
        assert_eq!(cwd, base.join("sub").display().to_string());
    }

    #[test]
    fn a_session_directory_flag_that_does_not_exist_is_an_error() {
        // Act
        let failure = session_cwd(Path::new("/"), Some(PathBuf::from("no/such/dir")));

        // Assert
        assert!(matches!(failure, Err(CliError::Path { .. })));
    }

    #[tokio::test]
    async fn a_cancel_the_daemon_cannot_answer_is_ignored() {
        // Arrange
        let dir = tempfile::Builder::new()
            .prefix("cc")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("d.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        let client = Client::connect_unchecked(&socket, coda_config::timing::Timing::for_tests())
            .await
            .unwrap();

        // Act
        cancel_turn(&client, &SessionId::new("s-1")).await;

        // Assert: reaching this line means the failure did not propagate or panic.
    }

    /// A daemon that serves one connection and returns the methods it was
    /// asked, ending after a `session.cancel` or when the connection closes.
    /// It leaves `session.send` unanswered unless `answer_send` is set.
    async fn serve(listener: tokio::net::UnixListener, answer_send: bool) -> Vec<String> {
        use coda_protocol::codec::{FrameDecoder, encode_frame};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut stream, _) = listener.accept().await.unwrap();
        let (mut decoder, mut methods, mut buffer) = (FrameDecoder::new(), Vec::new(), [0; 4096]);
        loop {
            let read = stream.read(&mut buffer).await.unwrap();
            if read == 0 {
                return methods;
            }
            decoder.push(&buffer[..read]).unwrap();
            while let Some(frame) = decoder.next_frame().unwrap() {
                let request: Value = serde_json::from_slice(&frame).unwrap();
                let method = request["method"].as_str().unwrap().to_owned();
                let result = match method.as_str() {
                    SESSION_SEND if answer_send => serde_json::json!({"turn_id": "t-1"}),
                    SESSION_CANCEL => serde_json::json!({"cancelled": true}),
                    _ => Value::Null,
                };
                methods.push(method.clone());
                if !result.is_null() {
                    let reply = serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
                    let frame = encode_frame(&serde_json::to_vec(&reply).unwrap()).unwrap();
                    stream.write_all(&frame).await.unwrap();
                }
                if method == SESSION_CANCEL {
                    return methods;
                }
            }
        }
    }

    async fn rig(
        answer_send: bool,
    ) -> (
        tempfile::TempDir,
        Client,
        tokio::task::JoinHandle<Vec<String>>,
    ) {
        let dir = tempfile::Builder::new()
            .prefix("cc")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("d.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let daemon = tokio::spawn(serve(listener, answer_send));
        let client = Client::connect_unchecked(&socket, coda_config::timing::Timing::for_tests())
            .await
            .unwrap();
        (dir, client, daemon)
    }

    fn no_second_connection() -> std::future::Ready<Result<Client, CliError>> {
        std::future::ready(Err(CliError::NotRunning))
    }

    fn params(session: &SessionId) -> SessionSendParams {
        SessionSendParams {
            session_id: session.clone(),
            text: "hello".to_owned(),
            max_turns: None,
            token_cap: None,
        }
    }

    #[tokio::test]
    async fn a_failed_second_connection_still_gets_the_cancel_over_the_main_one() {
        // Arrange
        let (_dir, client, daemon) = rig(true).await;
        let session = SessionId::new("s-1");
        let mut interrupts = signal(SignalKind::interrupt()).unwrap();
        let params = params(&session);
        let send = client.request::<_, SessionSendResult>(SESSION_SEND, &params);

        // Act
        let done = cancel_during_send(
            &client,
            &session,
            no_second_connection(),
            send,
            &mut interrupts,
        )
        .await;

        // Assert
        done.unwrap();
        let methods = tokio::time::timeout(Duration::from_secs(20), daemon)
            .await
            .expect("the daemon never saw the cancel")
            .unwrap();
        assert_eq!(methods, [SESSION_SEND, SESSION_CANCEL]);
    }

    #[tokio::test]
    async fn a_second_interrupt_ends_the_wait_for_the_send_at_once() {
        // Arrange
        let (_dir, client, _daemon) = rig(false).await;
        let session = SessionId::new("s-1");
        let mut interrupts = signal(SignalKind::interrupt()).unwrap();
        let params = params(&session);
        let send = client.request::<_, SessionSendResult>(SESSION_SEND, &params);
        let interrupt = std::process::Command::new("kill")
            .args(["-INT", &std::process::id().to_string()])
            .status()
            .unwrap();

        // Act
        let done = tokio::time::timeout(
            Duration::from_secs(10),
            cancel_during_send(
                &client,
                &session,
                no_second_connection(),
                send,
                &mut interrupts,
            ),
        )
        .await;

        // Assert
        assert!(interrupt.success());
        assert!(matches!(
            done.expect("the second interrupt was ignored"),
            Err(CliError::RunInterrupted)
        ));
    }
}
