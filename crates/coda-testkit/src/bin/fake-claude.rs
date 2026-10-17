// SPDX-License-Identifier: AGPL-3.0-only

//! A scripted stand-in for `claude -p --output-format stream-json`.
//!
//! Each user message on stdin produces one turn: a text delta, a permission
//! request for a `Bash` call, then a reply that depends on the answer. The
//! message shapes are synthetic, modelled on the stream-json protocol.

use std::io::{self, BufRead, Write};

const VERSION: &str = "2.1.292 (Claude Code)";
const SESSION_ID: &str = "fake-session-1";
const REQUEST_ID: &str = "req-1";
const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";
const CLAUDECODE_VAR: &str = "CLAUDECODE";

fn emit(out: &mut impl Write, value: &serde_json::Value) -> io::Result<()> {
    writeln!(out, "{value}")?;
    out.flush()
}

fn denied(response_line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(response_line)
        .is_ok_and(|v| v["response"]["response"]["behavior"] == "deny")
}

fn run_turn(
    lines: &mut impl Iterator<Item = io::Result<String>>,
    out: &mut impl Write,
) -> io::Result<()> {
    emit(
        out,
        &serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta",
            "delta":{"type":"text_delta","text":"working"}}}),
    )?;
    emit(
        out,
        &serde_json::json!({"type":"control_request","request_id":REQUEST_ID,
            "request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}),
    )?;
    let Some(response) = lines.next().transpose()? else {
        return Ok(());
    };
    let text = if denied(&response) { "denied" } else { "ran" };
    emit(
        out,
        &serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}}),
    )?;
    emit(out, &serde_json::json!({"type":"result","is_error":false}))
}

fn main() -> io::Result<()> {
    if std::env::args().any(|arg| arg == "--version") {
        println!("{VERSION}");
        return Ok(());
    }
    let mut out = io::stdout().lock();
    emit(
        &mut out,
        &serde_json::json!({"type":"system","subtype":"env_report",
            "anthropic_api_key_present":std::env::var_os(API_KEY_VAR).is_some(),
            "claudecode_present":std::env::var_os(CLAUDECODE_VAR).is_some()}),
    )?;
    emit(
        &mut out,
        &serde_json::json!({"type":"system","subtype":"init","session_id":SESSION_ID}),
    )?;
    let mut lines = io::stdin().lock().lines();
    while let Some(line) = lines.next().transpose()? {
        if !line.trim().is_empty() {
            run_turn(&mut lines, &mut out)?;
        }
    }
    Ok(())
}
