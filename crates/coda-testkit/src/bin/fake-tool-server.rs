// SPDX-License-Identifier: Apache-2.0

//! A scripted tool server that speaks the stdio protocol: one JSON-RPC message
//! per line. The shapes are SYNTHETIC, written from the protocol's documented
//! method names, not recorded from a real server.
//!
//! The first argument is a mode. `default` offers `echo`, `show_env`, `boom`,
//! `hang`, `fail`, `bigout` and `nontext`. `odd-tools` offers one good tool and
//! tools coda must refuse. `many-tools` offers 40. `silent` never answers the
//! handshake and `startup-crash` exits at once. `chatty` makes a request of its
//! own before it answers anything, `deaf` stops reading once it has listed its tools, `bigline` answers with a line over the limit
//! and `badjson` answers with text that is not JSON.
//!
//! The tools: `echo` returns its `text`; `show_env` returns the sorted names of
//! its environment variables; `boom` exits without answering; `hang` starts a
//! `sleep` process, writes its pid to the file named by the second argument and
//! never answers; `fail` answers with `isError`; `bigout` answers with 200 KiB;
//! `nontext` answers with an image part.

use std::io::{self, BufRead, Write};
use std::process::Command;

use serde_json::{Value, json};

const BIG_OUTPUT_BYTES: usize = 200 * 1024;
const BIG_LINE_BYTES: usize = 2 * 1024 * 1024;
const MANY_TOOLS: usize = 40;
const GRANDCHILD_SECONDS: &str = "300";
const CRASH_EXIT_CODE: i32 = 3;
/// The coverage runtime sets its own variable inside every process it instruments.
const COVERAGE_RUNTIME_PREFIX: &str = "__LLVM_PROFILE";

fn tool(name: &str, schema: &Value) -> Value {
    json!({"name": name, "description": format!("The {name} tool."), "inputSchema": schema})
}

fn object() -> Value {
    json!({"type": "object", "properties": {"text": {"type": "string"}}})
}

fn tools_for(mode: &str) -> Vec<Value> {
    match mode {
        "odd-tools" => vec![
            tool("good", &object()),
            tool("bash", &object()),
            tool("bad name", &object()),
            tool("not_object", &json!({"type": "string"})),
            tool(
                "huge_schema",
                &json!({"type": "object", "properties": {"a": {"description": "x".repeat(9000)}}}),
            ),
            json!({
                "name": "long_text",
                "description": "d".repeat(5000),
                "inputSchema": object(),
            }),
            tool("good", &object()),
        ],
        "many-tools" => (0..MANY_TOOLS)
            .map(|i| tool(&format!("t{i}"), &object()))
            .collect(),
        _ => [
            "echo", "show_env", "boom", "hang", "fail", "bigout", "nontext",
        ]
        .iter()
        .map(|name| tool(name, &object()))
        .collect(),
    }
}

fn reply(out: &mut impl Write, id: &Value, result: &Value) -> io::Result<()> {
    writeln!(
        out,
        "{}",
        json!({"jsonrpc": "2.0", "id": id, "result": result})
    )?;
    out.flush()
}

fn text(body: &str) -> Value {
    json!({"content": [{"type": "text", "text": body}]})
}

fn env_names() -> String {
    let mut names: Vec<String> = std::env::vars_os()
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .filter(|name| !name.starts_with(COVERAGE_RUNTIME_PREFIX))
        .collect();
    names.sort();
    names.join(",")
}

fn call(
    out: &mut impl Write,
    id: &Value,
    params: &Value,
    pid_file: Option<&str>,
) -> io::Result<()> {
    let name = params["name"].as_str().unwrap_or_default();
    let arguments = &params["arguments"];
    let result = match name {
        "echo" => text(&format!(
            "echo: {}",
            arguments["text"].as_str().unwrap_or("")
        )),
        "show_env" => text(&env_names()),
        "boom" => std::process::exit(CRASH_EXIT_CODE),
        "hang" => {
            if let (Ok(child), Some(path)) = (
                Command::new("sleep").arg(GRANDCHILD_SECONDS).spawn(),
                pid_file,
            ) {
                std::fs::write(path, child.id().to_string())?;
            }
            loop {
                std::thread::park();
            }
        }
        "fail" => json!({"isError": true, "content": [{"type": "text", "text": "nope"}]}),
        "bigout" => text(&"x".repeat(BIG_OUTPUT_BYTES)),
        "nontext" => {
            json!({"content": [{"type": "image", "data": "AAAA", "mimeType": "image/png"}]})
        }
        other => text(&format!("unknown tool {other}")),
    };
    reply(out, id, &result)
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map_or("default", String::as_str);
    let pid_file = args.get(2).map(String::as_str);
    match mode {
        "startup-crash" => std::process::exit(CRASH_EXIT_CODE),
        "silent" => loop {
            std::thread::park();
        },
        _ => {}
    }
    let stdin = io::stdin();
    let mut out = io::stdout();
    for line in stdin.lock().lines() {
        let message: Value = serde_json::from_str(&line?).unwrap_or(Value::Null);
        let (Some(method), Some(id)) = (message["method"].as_str(), message.get("id")) else {
            continue;
        };
        if mode == "chatty" {
            writeln!(
                out,
                "{}",
                json!({"jsonrpc": "2.0", "id": 900, "method": "ping"})
            )?;
            writeln!(
                out,
                "{}",
                json!({"jsonrpc": "2.0", "method": "notifications/message"})
            )?;
        }
        match (method, mode) {
            (_, "bigline") => writeln!(out, "{}", "x".repeat(BIG_LINE_BYTES))?,
            (_, "badjson") => writeln!(out, "this is not json")?,
            ("initialize", _) => reply(
                &mut out,
                id,
                &json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake-tool-server", "version": "0.0.0"},
                }),
            )?,
            ("tools/list", "deaf") => {
                reply(&mut out, id, &json!({"tools": tools_for(mode)}))?;
                loop {
                    std::thread::park();
                }
            }
            ("tools/list", _) => reply(&mut out, id, &json!({"tools": tools_for(mode)}))?,
            ("tools/call", _) => call(&mut out, id, &message["params"], pid_file)?,
            _ => {}
        }
        out.flush()?;
    }
    Ok(())
}
