// SPDX-License-Identifier: AGPL-3.0-only

//! A scripted stand-in for `codex exec --json`.
//!
//! One run is one turn: the prompt arrives on stdin, then the binary prints
//! `thread.started`, `turn.started`, an agent message, a command item and
//! `turn.completed`. The event shapes are SYNTHETIC, written from the
//! documented JSONL event names, not recorded from a real `codex` run.
//!
//! A prompt containing `crash` exits mid-turn and `fail` ends the turn with
//! `turn.failed`. `noise` is preceded by a malformed line and an unknown
//! event type, `bigline` by one line over the parser limit. `hostile` reports a thread id that looks like a flag. `stall` parks
//! the process after `turn.started` and `grandchild` first starts a `sleep`
//! process.
//!
//! `fake-codex app-server --listen unix://PATH` serves one WebSocket connection
//! instead (see [`app_server`]). Its prompts are `crash`, `stall` or `grandchild`
//! (as above), `closemid`, `bigframe`, `ping`, `fragment`, `nonobject`, `approve-command`,
//! `approve-file`, `approve-nocmd`, `approve-unknown`, `approve-heartbeat`, `approve-close`
//! and `other-request`, and its wire shapes are SYNTHETIC too.
//!
//! Test controls come from `fake-codex-control.json` in the working
//! directory, because the adapter passes the child only an allowlisted
//! environment. `report_path` receives the arguments, the prompt and
//! environment facts, and `pid_path` receives the `sleep` pid. `upgrade_status` makes the app-server answer the
//! upgrade with that HTTP status and `version` sets the version it reports. `no_thread_id` makes `thread/start`
//! answer without a thread id, `exit_early` exits before the socket exists and `hang_start` parks
//! before it creates one.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const VERSION: &str = "codex-cli 0.0.0-fake";
const NEW_THREAD_ID: &str = "fake-thread-1";
const RESUME_ARG: &str = "resume";
const SANDBOX_FLAG: &str = "--sandbox";
const HOSTILE_THREAD_ID: &str = "--dangerously-bypass-approvals-and-sandbox";
const CONTROL_FILE: &str = "fake-codex-control.json";
const GRANDCHILD_PROGRAM: &str = "sleep";
const GRANDCHILD_SECONDS: &str = "300";
const BIG_LINE_BYTES: usize = 9 * 1024 * 1024;
const PARK_INTERVAL: Duration = Duration::from_secs(3600);
const CRASH_EXIT_CODE: i32 = 3;

fn emit(out: &mut impl Write, value: &Value) -> io::Result<()> {
    writeln!(out, "{value}")?;
    out.flush()
}

fn control_path(key: &str) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(CONTROL_FILE).ok()?;
    let control: Value = serde_json::from_str(&raw).ok()?;
    control[key].as_str().map(PathBuf::from)
}

fn start_grandchild() -> io::Result<()> {
    let child = Command::new(GRANDCHILD_PROGRAM)
        .arg(GRANDCHILD_SECONDS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    match control_path("pid_path") {
        Some(path) => std::fs::write(path, child.id().to_string()),
        None => Ok(()),
    }
}

fn park() -> ! {
    loop {
        std::thread::sleep(PARK_INTERVAL);
    }
}

fn arg_after(args: &[String], name: &str) -> Option<String> {
    let at = args.iter().position(|arg| arg == name)?;
    args.get(at + 1).cloned()
}

fn write_report(args: &[String], prompt: &str) -> io::Result<()> {
    let Some(path) = control_path("report_path") else {
        return Ok(());
    };
    let report = json!({
        "pid": std::process::id(),
        "args": args,
        "prompt": prompt,
        "sandbox": arg_after(args, SANDBOX_FLAG),
        "env_names": std::env::vars_os()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        "home": std::env::var("HOME").ok(),
    });
    std::fs::write(path, report.to_string())
}

fn run_turn(args: &[String], prompt: &str, out: &mut impl Write) -> io::Result<()> {
    let thread_id = if prompt.contains("hostile") {
        HOSTILE_THREAD_ID.to_owned()
    } else {
        arg_after(args, RESUME_ARG).unwrap_or_else(|| NEW_THREAD_ID.to_owned())
    };
    if prompt.contains("grandchild") {
        start_grandchild()?;
    }
    if prompt.contains("noise") {
        writeln!(out, "{{not json")?;
        emit(out, &json!({"type": "mystery"}))?;
    }
    if prompt.contains("bigline") {
        writeln!(out, "{}", "x".repeat(BIG_LINE_BYTES))?;
    }
    emit(
        out,
        &json!({"type": "thread.started", "thread_id": thread_id}),
    )?;
    emit(out, &json!({"type": "turn.started"}))?;
    if prompt.contains("crash") {
        std::process::exit(CRASH_EXIT_CODE);
    }
    if prompt.contains("stall") || prompt.contains("grandchild") {
        park();
    }
    if prompt.contains("fail") {
        return emit(
            out,
            &json!({"type": "turn.failed", "error": {"message": "synthetic failure"}}),
        );
    }
    emit(
        out,
        &json!({"type": "item.completed", "item": {"id": "item_0", "type": "agent_message",
            "text": "working on it"}}),
    )?;
    emit(
        out,
        &json!({"type": "item.completed", "item": {"id": "item_1", "type": "command_execution",
            "command": "ls", "aggregated_output": "", "exit_code": 0, "status": "completed"}}),
    )?;
    emit(
        out,
        &json!({"type": "turn.completed", "usage": {"input_tokens": 120,
            "cached_input_tokens": 100, "output_tokens": 30}}),
    )
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version") {
        println!("{VERSION}");
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("app-server") {
        return app_server::serve(&args);
    }
    let mut prompt = String::new();
    io::stdin().lock().read_to_string(&mut prompt)?;
    write_report(&args, &prompt)?;
    run_turn(&args, &prompt, &mut io::stdout().lock())
}

/// The WebSocket mode: a one-connection server over a Unix socket.
mod app_server {
    use std::io::{self, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use super::{CRASH_EXIT_CODE, arg_after, control_path, park, start_grandchild, write_report};

    /// Every wait is bounded so an orphaned fake cannot outlive a test run.
    const LIFETIME: Duration = Duration::from_secs(120);
    const ACCEPT_POLL: Duration = Duration::from_millis(10);
    const LISTEN_FLAG: &str = "--listen";
    const SCHEME: &str = "unix://";
    const HEAD_END: &[u8] = b"\r\n\r\n";
    const UPGRADE_OK: &str = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
        Connection: Upgrade\r\n\r\n";
    const EARLY_EXIT_CODE: i32 = 5;
    const FILE_METHOD: &str = "item/fileChange/requestApproval";
    const SERVER_VERSION: &str = "0.0.0-fake";
    const OP_TEXT: u8 = 0x1;
    const OP_CLOSE: u8 = 0x8;
    const OP_PING: u8 = 0x9;
    const OP_PONG: u8 = 0xA;
    const OP_CONTINUATION: u8 = 0x0;
    const FIN: u8 = 0x80;
    const CLOSE_NORMAL: [u8; 2] = 1000_u16.to_be_bytes();
    /// Header of a binary frame announcing a payload far over any sane limit.
    const OVERSIZED_HEADER: [u8; 10] = [FIN | 0x2, 127, 0, 0, 0, 0, 0x7F, 0xFF, 0xFF, 0xFF];

    fn control_value(key: &str) -> Option<Value> {
        let raw = std::fs::read_to_string(super::CONTROL_FILE).ok()?;
        serde_json::from_str::<Value>(&raw).ok()?.get(key).cloned()
    }

    fn update_report(key: &str, value: &Value) -> io::Result<()> {
        let Some(path) = control_path("report_path") else {
            return Ok(());
        };
        let mut report: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        report[key] = value.clone();
        std::fs::write(path, report.to_string())
    }

    fn read_head(stream: &mut UnixStream) -> io::Result<()> {
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(HEAD_END) {
            stream.read_exact(&mut byte)?;
            head.push(byte[0]);
        }
        Ok(())
    }

    fn write_frame(stream: &mut UnixStream, first: u8, payload: &[u8]) -> io::Result<()> {
        let mut frame = vec![first];
        let len = u16::try_from(payload.len()).map_err(io::Error::other)?;
        match u8::try_from(len) {
            Ok(short) if short < 126 => frame.push(short),
            _ => {
                frame.push(126);
                frame.extend_from_slice(&len.to_be_bytes());
            }
        }
        frame.extend_from_slice(payload);
        stream.write_all(&frame)
    }

    fn send(stream: &mut UnixStream, value: &Value) -> io::Result<()> {
        write_frame(stream, FIN | OP_TEXT, value.to_string().as_bytes())
    }

    /// Reads one masked client frame; the fake's peer never fragments.
    fn read_frame(stream: &mut UnixStream) -> io::Result<(u8, Vec<u8>)> {
        let mut head = [0_u8; 2];
        stream.read_exact(&mut head)?;
        let len = match head[1] & 0x7F {
            126 => {
                let mut b = [0_u8; 2];
                stream.read_exact(&mut b)?;
                usize::from(u16::from_be_bytes(b))
            }
            127 => {
                let mut b = [0_u8; 8];
                stream.read_exact(&mut b)?;
                usize::try_from(u64::from_be_bytes(b)).map_err(io::Error::other)?
            }
            short => usize::from(short),
        };
        let mut mask = [0_u8; 4];
        stream.read_exact(&mut mask)?;
        let mut payload = vec![0_u8; len];
        stream.read_exact(&mut payload)?;
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
        Ok((head[0] & 0x0F, payload))
    }

    fn read_json(stream: &mut UnixStream) -> io::Result<Value> {
        loop {
            let (opcode, payload) = read_frame(stream)?;
            if opcode == OP_TEXT {
                return serde_json::from_slice(&payload).map_err(io::Error::other);
            }
        }
    }

    fn notify(stream: &mut UnixStream, method: &str, params: &Value) -> io::Result<()> {
        send(stream, &json!({"method": method, "params": params}))
    }

    fn accept(path: &str) -> io::Result<UnixStream> {
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        let deadline = Instant::now() + LIFETIME;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    stream.set_read_timeout(Some(LIFETIME))?;
                    return Ok(stream);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline {
                        return Err(error);
                    }
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn serve(args: &[String]) -> io::Result<()> {
        write_report(args, "")?;
        if control_value("exit_early").is_some() {
            std::process::exit(EARLY_EXIT_CODE);
        }
        if control_value("hang_start").is_some() {
            park();
        }
        let listen = arg_after(args, LISTEN_FLAG).unwrap_or_default();
        let mut stream = accept(listen.strip_prefix(SCHEME).unwrap_or(&listen))?;
        read_head(&mut stream)?;
        if let Some(status) = control_value("upgrade_status").and_then(|v| v.as_u64()) {
            let head = format!("HTTP/1.1 {status} Refused\r\nContent-Length: 0\r\n\r\n");
            return stream.write_all(head.as_bytes());
        }
        stream.write_all(UPGRADE_OK.as_bytes())?;
        loop {
            let Ok(message) = read_json(&mut stream) else {
                return Ok(());
            };
            let (id, params) = (message["id"].clone(), &message["params"]);
            match message["method"].as_str().unwrap_or_default() {
                "initialize" => {
                    let version = control_value("version")
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_else(|| SERVER_VERSION.to_owned());
                    send(
                        &mut stream,
                        &json!({"id": id, "result": {"serverInfo": {"name": "codex-app-server", "version": version}}}),
                    )?;
                }
                "thread/start" => {
                    update_report("thread_start", params)?;
                    let thread = if control_value("no_thread_id").is_some() {
                        json!({})
                    } else {
                        json!({"id": "fake-thread-1"})
                    };
                    send(
                        &mut stream,
                        &json!({"id": id, "result": {"thread": thread}}),
                    )?;
                }
                "turn/start" => {
                    send(
                        &mut stream,
                        &json!({"id": id, "result": {"turn": {"id": "turn-1"}}}),
                    )?;
                    let prompt = params["input"][0]["text"].as_str().unwrap_or_default();
                    run_turn(&mut stream, prompt)?;
                }
                _ => {}
            }
        }
    }

    fn delta(stream: &mut UnixStream, text: &str) -> io::Result<()> {
        notify(stream, "item/agentMessage/delta", &json!({"delta": text}))
    }

    fn complete(stream: &mut UnixStream) -> io::Result<()> {
        let usage =
            json!({"last": {"inputTokens": 120, "cachedInputTokens": 100, "outputTokens": 30}});
        notify(
            stream,
            "thread/tokenUsage/updated",
            &json!({"tokenUsage": usage}),
        )?;
        notify(
            stream,
            "turn/completed",
            &json!({"turn": {"status": "completed"}}),
        )
    }

    fn approval(stream: &mut UnixStream, method: &str, item: &Value) -> io::Result<()> {
        notify(stream, "item/started", &json!({"item": item}))?;
        ask(stream, method, &item["id"])?;
        respond(stream)?;
        notify(stream, "item/completed", &json!({"item": item}))
    }

    fn ask(stream: &mut UnixStream, method: &str, item_id: &Value) -> io::Result<()> {
        let request = json!({"id": "approval-1", "method": method, "params": {"itemId": item_id}});
        send(stream, &request)
    }

    fn respond(stream: &mut UnixStream) -> io::Result<()> {
        let response = read_json(stream)?;
        update_report("approval_response", &response["result"])
    }

    /// Sends a close frame, records whether the client answered it with one, and exits.
    fn close_and_exit(stream: &mut UnixStream) -> io::Result<()> {
        write_frame(stream, FIN | OP_CLOSE, &CLOSE_NORMAL)?;
        let (opcode, _) = read_frame(stream)?;
        update_report("close_reply", &json!(opcode == OP_CLOSE))?;
        std::process::exit(0);
    }

    fn run_turn(stream: &mut UnixStream, prompt: &str) -> io::Result<()> {
        if prompt.contains("crash") {
            std::process::exit(CRASH_EXIT_CODE);
        }
        if prompt.contains("closemid") {
            return close_and_exit(stream);
        }
        if prompt.contains("bigframe") {
            stream.write_all(&OVERSIZED_HEADER)?;
            return Ok(());
        }
        if prompt.contains("stall") || prompt.contains("grandchild") {
            if prompt.contains("grandchild") {
                start_grandchild()?;
            }
            park();
        }
        if prompt.contains("ping") {
            write_frame(stream, FIN | OP_PING, b"hb")?;
            let (opcode, payload) = read_frame(stream)?;
            update_report("pong_seen", &json!(opcode == OP_PONG))?;
            update_report("pong_payload", &json!(String::from_utf8_lossy(&payload)))?;
        }
        if prompt.contains("nonobject") {
            for junk in ["[]", "1", "\"x\""] {
                write_frame(stream, FIN | OP_TEXT, junk.as_bytes())?;
            }
        }
        if prompt.contains("other-request") {
            let request = json!({"id": "other-1", "method": "item/tool/call", "params": {}});
            send(stream, &request)?;
            let response = read_json(stream)?;
            update_report("other_response", &response["error"])?;
        }
        if prompt.contains("fragment") {
            write_frame(stream, OP_TEXT, br#"{"method":"item/agentMessage/delta","#)?;
            write_frame(
                stream,
                OP_CONTINUATION,
                br#""params":{"delta":"fragmented "#,
            )?;
            write_frame(stream, FIN | OP_CONTINUATION, br#"text"}}"#)?;
        } else {
            delta(stream, "working on it")?;
        }
        if prompt.contains("approve-command") {
            let item = json!({"id": "item-1", "type": "commandExecution",
                "command": "rm -rf build", "cwd": "/work"});
            approval(stream, "item/commandExecution/requestApproval", &item)?;
        }
        if prompt.contains("approve-nocmd") {
            let item = json!({"id": "item-3", "type": "commandExecution"});
            approval(stream, "item/commandExecution/requestApproval", &item)?;
        }
        if prompt.contains("approve-unknown") {
            ask(stream, FILE_METHOD, &json!("ghost"))?;
            respond(stream)?;
        }
        if prompt.contains("approve-heartbeat") {
            let item = json!({"id": "item-4", "type": "commandExecution", "command": "ls"});
            notify(stream, "item/started", &json!({"item": item}))?;
            ask(stream, "item/commandExecution/requestApproval", &item["id"])?;
            write_frame(stream, FIN | OP_PING, b"during")?;
            let (opcode, payload) = read_frame(stream)?;
            update_report(
                "pong_during_approval",
                &json!(opcode == OP_PONG && payload == b"during"),
            )?;
            respond(stream)?;
        }
        if prompt.contains("approve-close") {
            let item = json!({"id": "item-5", "type": "commandExecution", "command": "ls"});
            notify(stream, "item/started", &json!({"item": item}))?;
            ask(stream, "item/commandExecution/requestApproval", &item["id"])?;
            return close_and_exit(stream);
        }
        if prompt.contains("approve-file") {
            let item = json!({"id": "item-2", "type": "fileChange",
                "changes": [{"path": "src/a.rs"}, {"path": "src/b.rs"}]});
            approval(stream, "item/fileChange/requestApproval", &item)?;
        }
        complete(stream)
    }
}
