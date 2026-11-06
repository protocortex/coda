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
| `token_cap` | integer | `1000000` | Most tokens in one run. `coda run --token-cap` overrides it. A conversation the daemon builds again, after a restart, after it dropped an idle one to make room, or after a change to the skills, agents or trust decision, continues from the tokens the session already spent. |

## `[permissions]`

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `mode` | string | `"prompt"` | One of `"prompt"` (ask for each request), `"deny-all"` (refuse every request), `"accept-edits"` (allow file edits and `write`, ask for the rest). Any other value is an error. |
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
| `max_output_tokens` | integer | unset (8192) | Most tokens the model may write in one reply. Used by `anthropic` and `openai_compat`. Must be above 0; zero is an error that names `providers.<name>.max_output_tokens`. Lower it for a local model whose context is near 8192 tokens, because some servers refuse a request when the prompt plus this limit does not fit. |
| `max_tokens_field` | string | `"max_tokens"` | Request field that carries `max_output_tokens`: `"max_tokens"` or `"max_completion_tokens"`. OpenAI's o-series models accept only the second. Used by `openai_compat`. Any other value is an error that names `providers.<name>.max_tokens_field`. |
| `stream_usage` | boolean | `true` | Whether the request asks the server to report token counts at the end of the stream (`stream_options.include_usage`). Set it to `false` for a server that rejects that option. The server may then send no token counts, and the session's token budget does not count that provider's use. Used by `openai_compat`. |

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

A tier with no entry uses the model of the session that starts the agent, which is the model you switched to with `/model`, if you did. An entry must name an alias defined under `[models]`, and a key other than the three tiers is an error. Both are checked when the file loads. The `task` tool runs an agent on the model its tier maps to; the alias must use the same provider as the session that starts it, and `coda doctor` warns when an alias uses another provider than the default session provider (the one `coda run` starts on). See [agents.md](agents.md).

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

## `[review]`

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `max_findings` | integer | `5` | Most findings one review run reports. When the model returns more, the most severe are kept and the rest are counted in the run details. Must not be zero. |
| `diff_budget_chars` | integer | `120000` | Most characters of diff text one review sends to the model. The diff is packed to fit: binary, vendored and ignored files and deletion-only hunks are dropped, files of the repository's main language go first and smaller files before larger ones, and each hunk carries three lines of context and its new-side line numbers. Packing ends at the first file the budget cuts: that file keeps the hunks that fit whole, then the lines of the next hunk that fit (cut between lines, so one very large hunk still gives the model its first lines), and every later file is left out. A file where not even its header and the lines up to its first added line fit is left out too, and so are the files after it. A single line longer than the room that is left is not cut. The review's coverage list names every file as reviewed, clipped or left out. The `coda review` command applies it. Must not be zero. |

## `coda review`

`coda review [<target>]` reads a change, has the built-in `reviewer` agent look at it and prints the findings. It posts nothing and edits no file of the repository. The target is a pull request number or URL (read with the `gh` command, which uses your own `gh` login), a branch (against the default branch), a range such as `main...topic` (from the merge base; `main..` ends at `HEAD`), or nothing for the current branch against the default branch. All digits, or a URL with `/pull/`, is a pull request, so a branch named `42` cannot be reviewed by name; give it as a range. The default branch is where `origin/HEAD` points, else a local `main`, else a local `master`.

| Flag | Meaning |
| --- | --- |
| `coda review --staged` | Review the staged changes instead of a target. |
| `coda review --instructions <text>` | Extra instructions for the reviewer, from you. |
| `coda review --json` | Print the review as JSON, in the schema of the `coda-review` crate, instead of markdown. |
| `coda review --backend <name>` | `anthropic`. The reviewer runs in Coda's own agent loop through the `task` tool, so `claude-code` and `codex`, which run their own agents, are refused with an error. Without the flag, the faux provider is used if it is enabled, and the Anthropic API otherwise. |
| `coda review --cwd <dir>` | The directory to review in. |
| `coda review --force` | Review a pull request although a `[review.skip]` rule matches. |

The diff is packed to `[review] diff_budget_chars` and the ignore rules apply (see above). The packed text is written to `.coda/review/input-<pid>.txt` in the working directory, because it is too large to ask the model to copy into a tool call, and the reviewer reads it with its `read` tool. The file is removed when the command ends. The reviewer has only read-only tools: no `bash`, `edit` or `task`. Each finding must point at lines the diff shows; others are dropped and counted. If the answer is not in the review format, the reviewer is asked once more. A review is skipped (exit 0, one line printed) when a skip rule matches a pull request, and when the change has no lines to look at. A failure exits 70 with the reason on standard error and nothing on standard output. In the terminal UI, `/review [target]` asks the session's model to hand the change to the reviewer.

## `[review.ignore]`

Files a review leaves out.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `globs` | array of strings | `[]` | Globs matched against the path from the repository root. `*` stays inside one directory, `**/` and `/**` cross directories, `?` is one character. A glob with no `/` matches the file name at any depth, so `*.snap` ignores `a/b/c.snap`; it does not match the files under a directory of that name, so write `build/**` for those. A leading `/` pins the glob to the repository root, so `/docs/**` matches `docs/a.md` but not `x/docs/a.md`. A trailing `/` matches nothing. |
| `builtin` | boolean | `true` | Also leave out generated files: lockfiles (`Cargo.lock`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, `pnpm-lock.yaml`, `bun.lock`, `poetry.lock`, `Pipfile.lock`, `Gemfile.lock`, `composer.lock`, `go.sum`), minified files (`*.min.js`, `*.min.css`), source maps (`*.map`) and generated protobuf code (`*.pb.go`, `*_pb2.py`, `*_pb2_grpc.py`). |

## `[review.skip]`

Pull requests a review does not run on. Every rule is off until you set it, and one match skips the pull request. `coda review` applies it to a pull request: it prints a one-line reason and exits 0, and `--force` reviews anyway.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `draft` | boolean | `false` | Skip a draft. |
| `labels` | array of strings | `[]` | Skip a pull request with one of these labels. Case-insensitive. |
| `title_regex` | array of strings | `[]` | Skip when the title matches one of these regular expressions. |
| `source_branch_regex` | array of strings | `[]` | Skip when the branch the change comes from matches. |
| `target_branch_regex` | array of strings | `[]` | Skip when the branch it goes to matches. |
| `authors` | array of strings | `[]` | Skip when the author login is listed. Case-insensitive. |
| `bots` | boolean | `false` | Skip when the author is a bot account (a login ending in `[bot]`, or one the host marks as a bot). |

Regular expressions use the `regex` crate syntax and match anywhere in the text, so anchor them with `^` and `$` when you mean the whole value.

Review rules are read from the global `config.toml` and from `.coda/review.toml` on the repository's default branch (the library takes a `refs/heads/` or `refs/remotes/` ref such as `refs/remotes/origin/main`, so a tag with the branch's name cannot stand in for it), with the same `[review.ignore]` and `[review.skip]` tables. Lists from both add up, `draft` and `bots` are on when either turns them on, and the built-in ignore list stays on only if both keep it. A `.coda/review.toml` on the pull request's own branch is never read, so a pull request cannot switch off its own review.

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

`session_start` runs once for each turn loop of a session, on its first send, not once per send. The daemon keeps a session's loop between sends, so the hook runs again whenever the daemon builds the loop again from the stored history: after a daemon restart, after `/model` switches the session's model, after a change to the skills, the agents or the trust decision rebuilds the loop, and after an idle loop is dropped to make room or when the session is closed. `resumed` is true when a `session_start` hook already passed for the session. The daemon stores that as a `session_started` event when the hook passes, so a blocked first send, which leaves messages in the history, is still a first start when it is tried again, and a conversation that was started before any `session_start` hook was set reports `resumed` false the first time.

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

[review]
max_findings = 5
diff_budget_chars = 120000

[review.ignore]
globs = ["docs/generated/**"]
builtin = true

[review.skip]
draft = true
labels = ["skip-review"]
title_regex = ["^chore\\(deps\\)"]
source_branch_regex = ["^dependabot/"]
target_branch_regex = ["^experimental/"]
authors = ["release-bot"]
bots = true

[[hooks]]
event = "pre_tool"
command = ["/usr/local/bin/check-tool", "strict"]
blocking = true
timeout_ms = 5000
```
