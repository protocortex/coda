// SPDX-License-Identifier: Apache-2.0

//! `coda session list|send|show`: the stored sessions from a script.
//!
//! `send --wait` follows the turn like `coda run` and exits with the same
//! codes. `show` reads the conversation from the database, so it works whether
//! or not a daemon runs.

use std::process::ExitCode;

use coda_core::history::{ASSISTANT_MESSAGE_KIND, USER_MESSAGE_KIND};
use std::collections::HashSet;

use coda_protocol::methods::{
    SESSION_LIST, SESSION_REWIND, SESSION_SEND, SessionListParams, SessionListResult,
    SessionRewindParams, SessionRewindResult, SessionSendParams, SessionSendResult, SessionSummary,
};
use coda_protocol::types::{SessionId, TurnId};
use coda_store::{DB_FILE_NAME, Store, StoreError, StoredEvent};
use serde_json::{Value, json};

use crate::autostart::{connect_unchecked, ensure_daemon};
use crate::cli::{
    SessionCommand, SessionListArgs, SessionRewindArgs, SessionSendArgs, SessionShowArgs,
};
use crate::context::{Context, Io};
use crate::error::CliError;
use crate::run::{
    Delivery, STREAM_DELTA_KIND, Tap, finish_code, read_prompt, report, send_and_follow,
};

/// Events read from the database per query.
const EVENT_PAGE: u32 = 500;
/// The marker `show` prints before what the user said.
const USER_PREFIX: &str = "> ";
/// What `show` prints before the text of a turn a rewind rolled back.
const ROLLED_BACK_MARKER: &str = "[rolled back] ";
/// The `--to` value for before the first turn.
const BASE_TARGET: &str = "base";

/// Runs one `coda session` command and returns its exit code.
///
/// # Errors
/// Returns the errors of reaching the daemon and of its requests,
/// [`CliError::SessionNotFound`] for an id no session has, and
/// [`CliError::EmptyPrompt`] when `send` has nothing to send.
pub async fn run(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
    command: SessionCommand,
) -> Result<ExitCode, CliError> {
    match command {
        SessionCommand::List(args) => list(ctx, io, args).await.map(|()| ExitCode::SUCCESS),
        SessionCommand::Send(args) => send(ctx, io, force, args).await,
        SessionCommand::Show(args) => show(ctx, io, &args).map(|()| ExitCode::SUCCESS),
        SessionCommand::Rewind(args) => rewind(ctx, io, force, args)
            .await
            .map(|()| ExitCode::SUCCESS),
    }
}

async fn rewind(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
    args: SessionRewindArgs,
) -> Result<(), CliError> {
    let client = ensure_daemon(ctx, io, force).await?;
    let params = SessionRewindParams {
        session_id: SessionId::new(args.session),
        turn_id: (args.to != BASE_TARGET).then(|| TurnId::new(args.to)),
        keep_files: args.keep_files,
        keep_conversation: args.keep_conversation,
    };
    let done: SessionRewindResult = client.request(SESSION_REWIND, &params).await?;
    if args.json {
        writeln!(io.out, "{}", serde_json::to_string(&done)?)?;
    } else {
        let files = if done.files_restored {
            "restored the files"
        } else {
            "left the files"
        };
        let turns = if done.turns_rolled_back == 1 {
            "turn"
        } else {
            "turns"
        };
        writeln!(
            io.out,
            "{files}, rolled back {} {turns}",
            done.turns_rolled_back
        )?;
        if let Some(saved) = &done.saved_as {
            writeln!(
                io.out,
                "the files as they were are saved in {saved}; bring them back with `git restore --source {saved} --worktree -- .`"
            )?;
        }
    }
    Ok(())
}

async fn list(ctx: &Context, io: &mut Io<'_>, args: SessionListArgs) -> Result<(), CliError> {
    let client = connect_unchecked(ctx).await?;
    let cwd = args.cwd.map(|dir| ctx.cwd.join(dir).display().to_string());
    let params = SessionListParams {
        cwd,
        limit: args.limit,
    };
    let listed: SessionListResult = client.request(SESSION_LIST, &params).await?;
    if args.json {
        writeln!(io.out, "{}", serde_json::to_string(&listed.sessions)?)?;
    } else {
        for session in &listed.sessions {
            writeln!(io.out, "{}", list_line(session))?;
        }
    }
    Ok(())
}

fn list_line(session: &SessionSummary) -> String {
    let status = serde_json::to_value(session.status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    let backend = serde_json::to_value(session.backend)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    format!("{}  {status}  {backend}  {}", session.id, session.cwd)
}

async fn send(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
    args: SessionSendArgs,
) -> Result<ExitCode, CliError> {
    let prompt = read_prompt(io, args.prompt)?;
    let session = SessionId::new(args.session);
    let client = ensure_daemon(ctx, io, force).await?;
    if !args.wait {
        let params = SessionSendParams {
            session_id: session.clone(),
            text: prompt,
            max_turns: args.max_turns,
            token_cap: args.token_cap,
        };
        let sent: SessionSendResult = client.request(SESSION_SEND, &params).await?;
        if args.json {
            let out = json!({"session_id": session, "turn_id": sent.turn_id});
            writeln!(io.out, "{out}")?;
        } else {
            writeln!(io.out, "{}", sent.turn_id)?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    let delivery = Delivery {
        prompt,
        agent: None,
        max_turns: args.max_turns,
        token_cap: args.token_cap,
    };
    let mut text = String::new();
    let mut collect = |kind: &str, body: &Value| {
        if let (STREAM_DELTA_KIND, Some(delta)) = (kind, body["text"].as_str()) {
            text.push_str(delta);
        }
    };
    let mut tap = Tap {
        watch: &mut collect,
        print: !args.json,
    };
    let finish = send_and_follow(ctx, &client, &session, delivery, None, io, &mut tap).await?;
    let code = report(&finish, io)?;
    if args.json {
        let out = json!({
            "session_id": session,
            "stop": finish.condition,
            "failed": finish.failed,
            "exit_code": finish_code(&finish),
            "text": text,
        });
        writeln!(io.out, "{out}")?;
    }
    Ok(code)
}

fn show(ctx: &Context, io: &mut Io<'_>, args: &SessionShowArgs) -> Result<(), CliError> {
    let id = SessionId::new(args.session.clone());
    let not_found = || CliError::SessionNotFound {
        id: args.session.clone(),
    };
    if !ctx.dirs.data_dir().join(DB_FILE_NAME).exists() {
        return Err(not_found());
    }
    let store = Store::open(&ctx.dirs.data_dir())?;
    let record = store.load_session(&id).map_err(|e| match e {
        StoreError::SessionNotFound { .. } => not_found(),
        other => other.into(),
    })?;
    let mut events = Vec::new();
    let mut after = 0;
    loop {
        let page = store.load_events(&id, after, EVENT_PAGE)?;
        let Some(last) = page.last() else { break };
        after = last.seq;
        events.extend(page);
    }
    let rolled_back = store.rolled_back_turns(&id)?;
    if args.json {
        let out = json!({
            "session": {
                "id": record.id,
                "backend": record.backend,
                "model": record.model,
                "cwd": record.cwd,
                "mode": record.mode,
                "created_at": record.created_at,
            },
            "events": events.iter().map(|e| event_json(e, &rolled_back)).collect::<Vec<_>>(),
        });
        writeln!(io.out, "{out}")?;
    } else {
        for event in &events {
            if let Some(line) = transcript_line(event, &rolled_back) {
                writeln!(io.out, "{line}")?;
            }
        }
    }
    Ok(())
}

fn event_json(event: &StoredEvent, rolled_back: &HashSet<TurnId>) -> Value {
    let body =
        serde_json::from_str(&event.body).unwrap_or_else(|_| Value::String(event.body.clone()));
    json!({
        "seq": event.seq,
        "turn": event.turn,
        "kind": event.kind,
        "body": body,
        "at": event.at,
        "rolled_back": rolled_back.contains(&event.turn),
    })
}

/// What the user said, or what the model answered, as one block of text.
fn transcript_line(event: &StoredEvent, rolled_back: &HashSet<TurnId>) -> Option<String> {
    let body: Value = serde_json::from_str(&event.body).ok()?;
    let text = body["text"].as_str()?;
    let marker = if rolled_back.contains(&event.turn) {
        ROLLED_BACK_MARKER
    } else {
        ""
    };
    match event.kind.as_str() {
        USER_MESSAGE_KIND => Some(format!("{USER_PREFIX}{marker}{text}")),
        ASSISTANT_MESSAGE_KIND if !text.is_empty() => Some(format!("{marker}{text}")),
        _ => None,
    }
}
