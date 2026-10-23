# Configuration

Coda reads one file, `config.toml`, from its home directory.

## Creating the file

`coda init` writes a starter `config.toml` to this directory, with every provider section commented out. It refuses to overwrite a file that exists. Until a provider is enabled, `coda` and `coda tui` open the terminal UI with a note that tells you to enable one. A running daemon reads the file once, so run `coda daemon restart` after you edit it.

## Where files live

| Path | Holds |
| --- | --- |
| `$CODA_HOME/config.toml` | This file. |
| `$CODA_HOME/data/` | The database. |
| `$CODA_HOME/run/` | The daemon socket (`coda.sock`), lock and backend pidfiles. |
| `$CODA_HOME/daemon.log` | Error output of a daemon that Coda started in the background. |

`CODA_HOME` is the only environment override for the home directory. When it is unset, the home is `$HOME/.config/coda`. When neither is set, Coda stops with an error.

Keep the path short. A Unix socket path is limited to 104 bytes on macOS.

## Rules

- Every key is optional. An empty or missing file gives the defaults below.
- A key the schema does not know is not an error. It is reported as a warning (`coda doctor` shows it).
- A wrong type or a bad table is an error, and the file is rejected.
- Under `[providers]`, a key named like a secret (ending in `api_key`, `apikey`, `token`, `secret`, `password`, `passwd` or `credential`, with an optional trailing `s`) must not hold a string. Coda rejects the file and names the key. Keys ending in `_env` are exempt. Put the key in an environment variable and name it with `api_key_env`.

## Top level

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `max_turns` | integer | `50` | Most agent turns in one run. `coda run --max-turns` overrides it. |
| `token_cap` | integer | `1000000` | Most tokens in one run. `coda run --token-cap` overrides it. |

## `[permissions]`

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `mode` | string | `"prompt"` | One of `"prompt"` (ask for each request), `"deny-all"` (refuse every request), `"accept-edits"` (allow file edits, ask for the rest). Any other value is an error. |
| `grants` | array of strings | `[]` | Grants approved ahead of time. Each entry has the form `tool:pattern`, for example `"bash:ls"`, and the pattern may be empty. An entry without a colon, or with an empty tool, fails session start with an `InvalidGrant` error. `coda grant add` stores per-repository grants separately. |

## `[providers.<name>]`

One section per backend. The names Coda uses are `claude_code`, `codex`, `anthropic`, `openai_compat` and `faux`.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | boolean | `false` | Whether the backend may be used. Every backend is off until you turn it on. |
| `binary` | string (path) | unset | Path of the backend program. When unset, Coda searches the `PATH`. Used by `claude_code` and `codex`. |
| `api_key_env` | string | unset | Name of the environment variable that holds the API key. The anthropic provider requires it. Coda stores only the name. |
| `env_passthrough` | array of strings | `[]` | Extra environment variable names for the backend process. Read from the file, not used yet. |
| `verified_versions` | array of strings | `[]` | Backend versions you have checked. They are added to the versions built in for each backend: `claude_code` has `2.1.292` and `2.1.296` built in and `codex` has `0.163.0`. |
| `script` | string (path) | unset | Script file the `faux` provider replays. It is required for `faux`, and a file over its size limit is refused. |
| `request_log` | string (path) | unset | File the `faux` provider appends one JSON line to for each request it receives, so a test can see what the model was sent. The file holds conversation content and is created readable by its owner only. Use it in tests, not in daily use. |
| `base_url` | string | unset | API root. Required for `openai_compat`. The anthropic provider uses its own default when unset. |
| `model` | string | unset | Model a session uses when it names none. |
| `sandbox` | string | unset (read-only) | Sandbox for a Codex session: `"read-only"`, `"workspace-write"` or `"danger-full-access"`. Any other value is an error that names `providers.<name>.sandbox`. |
| `allow_wider_sandbox` | boolean | `false` | Must be `true` before a `sandbox` wider than read-only takes effect. |

`claude_code` and `codex` start the programs you installed, which may run under a subscription. Read the account risk notice in the README before you enable them.

A faux script is a JSON array. Each item is one turn with an `events` list, and an optional `error`:

```json
[{"events": [
  {"type": "text_delta", "text": "hello from faux"},
  {"type": "usage", "input_tokens": 11, "output_tokens": 7},
  {"type": "stop", "reason": "end_turn"}
]}]
```

## `[models.<alias>]`

A name that picks a provider and a model. Both keys are required in a section.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `provider` | string | none | The provider that serves the model: `anthropic`, `openai_compat` or `faux`. `claude_code` and `codex` are not accepted. |
| `model` | string | none | The model id sent to that provider. |
| `context_window` | integer | none | Optional. The model's context window in tokens. It replaces the built-in value for this provider and model. Must be above 0. Two aliases of the same provider and model must not set different values. A model that is neither listed nor given a value has no known window, so Coda compacts it only after the provider reports an overflow. |

The built-in defaults cover `claude-sonnet-5-5` and `claude-sonnet-4-5` (200000 tokens) and `gpt-4o` (128000 tokens).

## `[routing]`

Maps an agent tier to a `[models.<alias>]` name. Agent files name a tier, never a model id, so changing a model here changes every agent on that tier.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `fast` | string | unset | Alias for cheap, quick work such as search. |
| `standard` | string | unset | Alias for everyday implementation and review. |
| `deep` | string | unset | Alias for hard reasoning. |

A tier with no entry uses the default model. An entry must name an alias defined under `[models]`, and a key other than the three tiers is an error. Both are checked when the file loads. The `task` tool runs an agent on the model its tier maps to; the alias must use the same provider as the session that starts it. See [agents.md](agents.md).

## `[telemetry]`

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `otlp_endpoint` | string | unset | OTLP collector URL. Export is off when unset. |
| `capture_content` | boolean | `false` | Record prompt and response text. |

## `[timing]`

Each value is an integer number of milliseconds. A key left out keeps its default.

| Key | Default | Meaning |
| --- | --- | --- |
| `idle_timeout_ms` | `60000` | How long a backend may stay silent before it counts as stuck. Does not apply while a delegated backend runs a tool of its own; `delegated_tool_ms` does. |
| `retry_backoff_base_ms` | `500` | First delay between provider retries. |
| `retry_backoff_max_ms` | `30000` | Upper bound for the retry delay. |
| `restart_backoff_ms` | `1000` | Delay before a crashed backend restarts. |
| `permission_default_deny_ms` | `120000` | How long a permission prompt waits before it is denied. |
| `tool_execution_ms` | `300000` | Longest run time of one of coda's own tool calls (the tools coda runs itself, not a delegated backend's). |
| `delegated_tool_ms` | `900000` | How long a delegated backend (Claude Code, Codex) may stay silent while a tool runs, before the turn is aborted. It covers tools the backend started on its own and tool calls coda approved for it. Above Claude Code's 10 minute Bash maximum; must not be zero. |

## `[[hooks]]`

A hook is a program coda runs at a point in a model-API session (`anthropic`, `openai_compat`, `faux`). Hooks run in coda's own tool path. Claude Code and Codex run their own tools and coda only sees the calls it is asked to approve through its permission broker, so no coda hook runs for those sessions. When the config has hooks and a session runs on one of those backends, the terminal UI shows a notice when the session starts and `coda run` prints one line on standard error, so a guard is never assumed to be active. Hooks come from this file only. A project file or a definition file cannot add one.

An entry is a table in an array, so it has no single default. The keys:

| Key | Type | Meaning |
| --- | --- | --- |
| `event` | string | Required. `"session_start"` (before the first send of each turn loop, which can happen more than once in a conversation, see below), `"pre_tool"` (after a tool call is allowed, before the tool runs) or `"post_tool"` (after the tool ran, or crashed). |
| `command` | array of strings | Required. The program and its arguments. There is no shell, so a string is an error. Use `["/bin/sh", "-c", "..."]` for a shell. The program is an absolute path or a bare name. A bare name is looked up only in the absolute entries of the hook's `PATH`: an empty or relative entry is skipped, because it would find a file of the session directory. A relative path such as `./guard.sh` is an error that names the hook, because it would run a file of the session directory, which the model can edit. |
| `blocking` | boolean | Default `false`. When true, a failing hook stops the action. When false, a failure is logged and the action goes on. |
| `timeout_ms` | integer | Default `10000`, from 1 to `300000`. A hook that outlives it fails and its whole process group is killed. |

A hook runs in the session's directory with the environment bash gets (`PATH`, `HOME`, `LANG`, `LC_ALL`, `TERM`, `TZ`) and no API key. It gets one JSON object on standard input:

| Event | Fields |
| --- | --- |
| all | `event`, `session`, `cwd` |
| `session_start` | `resumed` (a `session_start` hook already passed for this session, in an earlier turn loop) |
| `pre_tool` | `tool`, `arguments` (the call's arguments as JSON, whole) |
| `post_tool` | `tool`, `is_error`, `result` (the text the model will see, whole) |

A hook gets a call whole up to 256 KiB of arguments (as JSON text) and 64 KiB of result, counted before the text is escaped inside the payload. A result above 16 KiB reaches the model compacted, so a guard sees every byte the model sees. Past those limits a blocking hook is not run: it fails closed and the call is stopped with a reason that names the part and its limit, for example `the result is too large for the hook to check (over 65536 bytes)`. A hook that is not blocking is run on a cut payload instead: `arguments` becomes the first 256 KiB as text and `arguments_truncated` is true, or `result` is cut and `result_truncated` is true.

A hook fails when it cannot start, exits with a status other than 0, or times out. What a failure does:

- A failing blocking `session_start` hook stops the turn before the model is called, and the next send tries it again. `coda run` prints the reason on standard error, prints nothing on standard output and exits 5 (the stop condition is `hook_blocked`). The terminal UI shows the reason as a notice. The reason is never sent to the model.
- A failing blocking `pre_tool` hook stops the tool. The model gets an error naming the hook and the reason.
- A failing blocking `post_tool` hook replaces the tool's result with that error. This holds for a tool that crashed too, and for a turn that is cancelled while the hook runs or just as the tool returns: with a blocking `post_tool` hook set, the model gets an error saying the result is withheld, not the unchecked result. A result withheld this way, or replaced by a hook's error, leaves no stored original for `retrieve` to return. With only hooks that are not blocking, a cancelled turn keeps the result.
- A failing hook that is not blocking is logged as `hook failed, continuing: event=<event> hook=hooks[<index>] (<program>) reason=<why>`.

A hook is named by its place in the `[[hooks]]` list, counting from 0, and its program, for example `hooks[2] (/bin/sh)`, so two shell hooks are told apart. The reason is how the hook failed (`exited with status 3`, `timed out after 5000 ms`, `could not start: ...`) and, when it wrote any, the last 1 KiB of its standard error after a colon. It is folded onto one line and has no control or format characters, so a hook cannot forge a log line, move the cursor of a terminal or hide text with a zero width or right-to-left character.

`session_start` runs once for each turn loop of a session, on its first send, not once per send. The daemon keeps a session's loop between sends, so the hook runs again whenever the daemon builds the loop again from the stored history: after a daemon restart, after `/model` switches the session's model, after a change to the skills or to the trust decision rebuilds the loop, and after an idle loop is dropped to make room or when the session is closed. `resumed` is true when a `session_start` hook already passed for the session. The daemon stores that as a `session_started` event when the hook passes, so a blocked first send, which leaves messages in the history, is still a first start when it is tried again, and a conversation that was started before any `session_start` hook was set reports `resumed` false the first time.

A sub-agent started by the `task` tool (see [agents.md](agents.md#dispatch-the-task-tool)) runs the `pre_tool` and `post_tool` hooks for its own tool calls, so a blocking hook stops a sub-agent's call as it stops the session's, and a sub-agent is no way around one. It runs no `session_start` hook: that event marks the start of the session, not of a sub-agent. The session's `task` call runs no hook, because dispatching asks no permission and does nothing on the machine; the sub-agent's tool calls are what the hooks see. The `session` in a sub-agent's payload is the id of the session that started it.

A hook that times out or whose turn is cancelled is killed together with everything it started (its process group). A hook that exits on its own is not followed: coda signals nothing after the hook's own process has ended, because its process group id may belong to someone else by then, so a process the hook left running keeps running. Only the end of the hook's standard error is read, and only for a failed hook, for at most half a second after it exits; what was read by then is the reason, even when a process the hook left behind still holds the pipe open.

A key the schema does not know in a hook entry is an error, so a misspelled `blocking` never turns a guard into a log line.

## Example

This file uses every key. It is checked against the parser when the tests run.

```toml coda-config
max_turns = 20
token_cap = 500000

[permissions]
mode = "accept-edits"
grants = ["bash:ls"]

[providers.claude_code]
enabled = false
binary = "/usr/local/bin/claude"
verified_versions = ["2.1.292"]
env_passthrough = ["HTTPS_PROXY"]

[providers.codex]
enabled = false
sandbox = "workspace-write"
allow_wider_sandbox = true

[providers.anthropic]
enabled = true
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-sonnet-5-5"

[providers.openai_compat]
enabled = true
base_url = "http://localhost:11434/v1"
api_key_env = "LOCAL_LLM_KEY"

[providers.faux]
enabled = true
script = "/path/to/script.json"

[models.local]
provider = "openai_compat"
model = "llama3"
context_window = 32000

[telemetry]
otlp_endpoint = "http://localhost:4318"
capture_content = false

[timing]
idle_timeout_ms = 60000
retry_backoff_base_ms = 500
retry_backoff_max_ms = 30000
restart_backoff_ms = 1000
permission_default_deny_ms = 120000
tool_execution_ms = 300000
delegated_tool_ms = 900000

[[hooks]]
event = "pre_tool"
command = ["/usr/local/bin/check-tool", "strict"]
blocking = true
timeout_ms = 5000
```
