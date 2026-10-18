// SPDX-License-Identifier: AGPL-3.0-only

//! `coda run`: sends one prompt to a new session, follows it to the end and
//! exits with a code that names why the turn stopped.
//!
//! The run is headless: nobody can answer a permission prompt, so every
//! prompt is denied and the model carries on without that tool call.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use coda_client::{Client, Subscription};
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
use tracing::{debug, info, warn};

use crate::autostart::ensure_daemon;
use crate::cli::RunArgs;
use crate::context::{Context, Io};
use crate::daemon_cmd::load_config;
use crate::error::CliError;
use crate::exit;

const FAUX_KEY: &str = "faux";
const STREAM_DELTA_KIND: &str = "stream_delta";
const RESULT_KIND: &str = "result";

const END_TURN: &str = StopCondition::EndTurn.as_str();
const MAX_TURNS: &str = StopCondition::MaxTurns.as_str();
const BUDGET_EXHAUSTED: &str = StopCondition::BudgetExhausted.as_str();
const CONTEXT_OVERFLOW: &str = StopCondition::ContextOverflow.as_str();
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
}

/// Runs the prompt, from `--prompt` or else standard input, in a new session
/// and returns the exit code for how it stopped.
///
/// # Errors
/// Returns [`CliError::EmptyPrompt`] when there is nothing to send,
/// [`CliError::RunInterrupted`] when the daemon connection is lost before the
/// turn ends, or the errors of reaching the daemon and of its requests.
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
    info!(step = "send", session = %session, "sending the prompt");
    let _: SessionSendResult = client
        .request(
            SESSION_SEND,
            &SessionSendParams {
                session_id: session.clone(),
                text: prompt,
                max_turns: args.max_turns,
                token_cap: args.token_cap,
            },
        )
        .await?;
    info!(step = "follow", session = %session, "following the turn");
    let finish = follow(&client, &mut events, &session, io).await?;
    Ok(report(&finish, io)?)
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

/// The scripted provider when the config enables it, else the Anthropic API.
fn backend(ctx: &Context) -> Result<BackendKind, CliError> {
    let config = load_config(&ctx.dirs)?;
    let scripted = config.providers.get(FAUX_KEY).is_some_and(|p| p.enabled);
    Ok(if scripted {
        BackendKind::Faux
    } else {
        BackendKind::Anthropic
    })
}

/// Prints streamed text, denies permission prompts and cancels on Ctrl-C,
/// until the turn's result arrives.
async fn follow(
    client: &Client,
    events: &mut Subscription,
    session: &SessionId,
    io: &mut Io<'_>,
) -> Result<Finish, CliError> {
    let mut condition = None;
    let mut printed = false;
    loop {
        let notification = tokio::select! {
            next = events.next() => next.ok_or(CliError::RunInterrupted)?,
            _ = tokio::signal::ctrl_c() => {
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
                        return Ok(Finish { condition, failed });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// The cancel can lose a race with the turn ending; the result still follows,
/// so a failed cancel is only logged.
async fn cancel_turn(client: &Client, session: &SessionId) {
    let params = SessionCancelParams {
        session_id: session.clone(),
    };
    let cancelled: Result<SessionCancelResult, _> = client.request(SESSION_CANCEL, &params).await;
    if let Err(failure) = cancelled {
        warn!(session = %session, error = %failure, "the cancel request failed; following the turn on");
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
    let code = match finish.condition.as_deref() {
        Some(condition) => exit_code(condition),
        None => exit::RUN_OTHER,
    };
    if code == exit::RUN_OTHER {
        let reason = match (&finish.condition, finish.failed) {
            (Some(condition), _) => format!("the run stopped: {condition}"),
            (None, true) => "the run failed; see the daemon log".to_owned(),
            (None, false) => "the run ended without a stop reason".to_owned(),
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
}
