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

## Steer

`session.send` with `mode: steer` rests on two probes, neither of which is a recorded
fixture. A manual run with claude 2.1.296 wrote `Use the Bash tool to run sleep 15`, then a second
`user` line on the same stream-json input seven seconds in. The tool result came back, and the
one `result` of the turn answered both messages. The schema that `codex app-server
generate-json-schema` writes for codex-cli 0.163.0 has `turn/steer` with a required
`expectedTurnId`, which coda sends; a live Codex turn was not run. Both used the
installed logins and about two short calls.

## Checks that a review cannot change files

`coda review` runs the reviewer on Claude Code and Codex in plan mode. Two live
tests check what that stops. `a_read_only_claude_session_cannot_write_a_file` starts
Claude Code the way a plan-mode session starts, with the editing and shell tools banned,
asks it to write a file and fails if the file exists. `codex_app_server_approval_declined`
fails if the file it asks Codex to create exists after the approval is denied. They run with
the others under the same switch, and the Claude one writes no fixture.

## Live probes after an upgrade

`crates/coda-providers/tests/live_probes.rs` holds opt-in probes of the installed
`claude`. Run them after every `claude` upgrade:

```sh
CODA_LIVE_TESTS=1 cargo nextest run -p coda-providers --test live_probes \
  --run-ignored only --test-threads 1
```

They use the login of whoever runs them, the unmodified binary, the same cleared
environment a session gets, the `haiku` model and a turn limit, so the seven tests
spend about eight short calls. They write no fixture, and they print no token and
no account value (only the names of the fields of the `initialize` account block).
Run them one at a time, as above, so they do not hit the plan's rate limit together.
Each call starts a real session, so `claude` keeps a transcript of it under
`~/.claude/projects`, and `coda usage --history` counts it. A probe fails before it
spends more if the session would bill API rates instead of the plan.
Every test is `#[ignore]` and needs `CODA_LIVE_TESTS=1`; CI never runs them. What
they found with claude 2.1.296 on 2026-11-08:

| Probe | Result |
| --- | --- |
| prompt size by profile | `full` 26.8k, `project` 22.3k, `lean` 5.2k tokens; fails above 25k for `project`, above 12k for `lean`, or when the order changes |
| init message | `project` and `lean` list no MCP server and no skill, and `lean` lists only its six tools (the same check coda runs on every session) |
| usage and rate limits | the usage event is read, and each `rate_limit_event` line becomes a `rate_limit` event |
| `initialize` | answers `success`; the account block has `apiProvider`, `email`, `organization` and `subscriptionType`, and no `apiKeySource` or `tokenSource` |
| `set_permission_mode`, `set_model` | both answer `success` with no turn running |
| `interrupt` | answers `success`; the turn then ends with a `result` of subtype `error_during_execution` and `is_error` true |
| a message sent mid-turn | in a turn that only streams text it queues: the first message gets its own `result` and the second gets another. The manual run under Steer, with a tool still running, gave one `result` for both, so the answer depends on the turn |
| `--fork-session` | with `--resume <id>` it starts a new session id and the turn works |
| resume at a message | `claude --help` lists no flag for it (`--resume-session-at`); the probe fails the day one appears |

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
