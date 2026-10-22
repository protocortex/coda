# Live transcripts

The fixtures named `live-*.jsonl` under `crates/coda-testkit/fixtures/claude/`
and `.../codex/` were recorded from the real `claude` and `codex` CLIs, run
through coda's own adapters. Files named `synthetic-*` are hand-written and prove
nothing about the real protocol. The first line of every fixture is a
`coda_recording` header with the label, the CLI version, the UTC time
(`recorded_at`), the recorder, the scenario and the prompt.

Recorded on 2026-10-21 with claude 2.1.296 and codex-cli 0.163.0, using the
user's existing subscription logins. The two app-server decline and cancel
transcripts were re-recorded on 2026-10-22 with the same codex-cli.

## Fixtures

| File | What it holds |
| --- | --- |
| `claude/live-allow.jsonl` | `touch hello.txt` asks permission, coda answers allow |
| `claude/live-deny.jsonl` | the same request, coda answers deny |
| `claude/live-readonly.jsonl` | `echo hello` runs with no permission request |
| `claude/live-text.jsonl` | a plain text turn |
| `codex/live-exec.jsonl` | two `codex exec --json` turns; the second resumes the thread |
| `codex/live-app-server-accept.jsonl` | app-server turn, command approval accepted |
| `codex/live-app-server-decline.jsonl` | the same turn, approval denied: coda answers `cancel` because the server offers no `decline`, and the turn ends `interrupted` |
| `codex/live-app-server-cancel.jsonl` | `sleep 30` approved, then the turn is cancelled: coda sends `turn/interrupt`, the server replies `{}` and ends the turn with `turn/completed` status `interrupted` |

A Claude transcript is the stream `claude` wrote, with each `control_response`
coda sent placed right after the `control_request` it answers. A Codex exec
transcript is the stdout of each process, in order. An app-server transcript has
one line per JSON-RPC text frame, `{"dir":"send"|"recv","message":{...}}`, plus a
`{"dir","http"}` line for the WebSocket upgrade.

## Recording again

```sh
CODA_LIVE_TESTS=1 scripts/live-record.sh
```

This spends quota on about eight short calls and is never run by CI. The tests in
`crates/coda-providers/tests/live_record.rs` are `#[ignore]` and also need
`CODA_LIVE_TESTS=1`. Each call has a time bound, runs in an empty temporary
directory with a short path, and starts the CLI through coda's adapter with its
cleared environment and allowlist. A shell shim between the adapter and the CLI
copies the traffic to files. The Claude shim also adds `--model haiku
--max-turns 2 --setting-sources project --strict-mcp-config
--disable-slash-commands`, so no user settings, hooks or MCP servers load and a
call stays cheap. The Codex shims set `CODEX_HOME` to a throwaway directory that holds a copy of
the login (deleted after the run) and an empty config, so the user's MCP servers,
plugins and skills do not load; the built-in `codex_apps` server still starts.
The app-server shim also moves the socket and a proxy in the test logs the
WebSocket text frames. The accept, decline and cancel recordings fail unless the gate saw
an approval request, and the cancel recording fails unless the turn ends cancelled. The Codex exec working directory is a fresh Git repository, because `codex exec` refuses to run
anywhere else.

## Scrubbing

The script scrubs every recording with `coda-selfhost scrub` before it writes a
fixture. The scrubber gets only `HOME`, the user name and the host name as
environment values. It replaces those values, absolute paths, `sk-` keys, email
addresses, UUIDs (numbered stand-ins, so equal ids stay equal), `prefix_body` ids
such as `msg_...` and `toolu_...`, and long mixed-case blobs such as thinking
signatures. It also zeroes quota numbers (`usedPercent`, `utilization`,
`resetsAt`, `windowDurationMins`, `balance`), redacts `authMode`, `planType` and
the overage fields, and cuts the OS and terminal out of `userAgent`. The scrubber (`scrub --jsonl`) fails if a scrubbed line is not valid
JSON. The script removes the fixtures in that case, or if they still contain
the user name, home path, host name, `/Users/`, `/home/`, an email address or a
key. Read the fixtures before committing them: the scrubber works on text, not on
meaning, so a new field can carry something it does not know.
