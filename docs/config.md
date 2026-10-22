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
```
