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
| `max_guard_continues` | integer | `2` | Most times an `end_turn` hook may send the model back to work in one send. With `0` no `end_turn` hook runs. See [hooks](#hooks). |
| `max_parallel_tools` | integer | `4` | Most read-only tool calls from one model reply that run at the same time. Must be at least `1`; with `1` every call runs in turn. See [parallel tool calls](#parallel-tool-calls). |

### Parallel tool calls

When one model reply holds several calls to read-only tools (`read`, `grep`, `list` and the `code_*` tools), they run at the same time, up to `max_parallel_tools` at once. A call to any other tool, such as `edit`, `write` or `bash`, never overlaps another call: it waits for the read-only calls before it and runs alone. Permission prompts and `pre_tool` hooks still go one call at a time, in the order the model wrote them, but for a run of read-only calls all of them come before the first call starts, so a `pre_tool` hook of a later call does not see what the `post_tool` hook of an earlier call did. `post_tool` hooks run in call order as each call finishes. A cancel stops the prompts for the calls not yet asked. Results are stored in that same order, so a replayed session reads the same.

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
| `thinking_budget_tokens` | integer | unset | Tokens the model may spend on extended thinking before it answers. When set, the request asks for thinking with this budget. Unset sends no thinking setting, so only models that think by default return thinking blocks. It must be at least 1024 and below `max_output_tokens` (8192 when that is unset); any other value is an error that names `providers.<name>.thinking_budget_tokens`. Thinking blocks are kept and sent back as described under reasoning below. Used by `anthropic`; the bounds are checked only for that section. The API refuses a thinking block with no signature, so a tool loop whose earlier replies came from another provider may fail with thinking on. |
| `max_tokens_field` | string | `"max_tokens"` | Request field that carries `max_output_tokens`: `"max_tokens"` or `"max_completion_tokens"`. OpenAI's o-series models accept only the second. Used by `openai_compat`. Any other value is an error that names `providers.<name>.max_tokens_field`. |
| `stream_usage` | boolean | `true` | Whether the request asks the server to report token counts at the end of the stream (`stream_options.include_usage`). Set it to `false` for a server that rejects that option. The server may then send no token counts, and the session's token budget does not count that provider's use. Used by `openai_compat`. |
| `text_tool_calls` | boolean | `false` | Whether a reply that makes no tool call, and whose whole text is one JSON object, counts as a call when the object fits exactly one tool. See below. Used by model-API providers. |

`claude_code` and `codex` start the programs you installed, which may run under a subscription. Read the account risk notice in the README before you enable them.

Tool-call recovery in model-API sessions. When a call's arguments are not valid JSON, coda tries a strict repair and, if it works, runs the call with the repaired arguments and shows a notice (stored as a `call_recovered` event). The repairs are: a trailing comma, single-quoted strings, a raw line break or tab inside a string, and closing braces or brackets cut off the end. Nothing else is repaired: an unquoted key, a string cut short, a missing value, or text around the JSON is returned to the model as an error that includes the tool's input schema, so it can try again. With `text_tool_calls = true`, a reply that makes no structured call is read as a call when its whole text is either one JSON object with every required key of one tool and no key that tool lacks, or `{"name": "<tool>", "arguments": {...}}` with arguments that fit the named tool. A tool name that is not registered, a fit with two tools, or any text around the JSON makes no call, and the reply ends the turn as before. Each call read this way is stored as a `call_recovered` event with `how` set to `from_text`. Repair also runs in sub-agents, and their `call_recovered` events are stored as child events; a call read from reply text is only for the session's own model, not for sub-agents. A cut-off that ends inside a number or a word is not repaired, because it would change the value.

Reasoning in model-API sessions. When a model returns reasoning, coda keeps it out of the answer text and sends it back with the next request where the provider needs it. The `anthropic` provider sends back each thinking block with its signature, and each redacted block as it came, before the text and tool calls of the same reply; a thinking block with no signature is left out because the API would refuse it. The `openai_compat` provider reads the `reasoning_content` and `reasoning` fields of a stream and sends the text back as `reasoning_content` only on a reply that made tool calls, because that is where servers that take it need it. A local server that prints its reasoning in `<think>` tags inside the text is handled too: when a reply begins with `<think>`, the text up to `</think>` is reasoning, even when a tag arrives split across chunks. Text that only looks like a tag, or a tag that does not open the reply, stays in the answer. The reasoning counts in the tokens the provider reports. It is stored in the session's originals, apart from the event log, and referenced from the `assistant_message` event, so a rebuilt conversation sends it back as the live one did. A session whose model gives no reasoning sends and stores exactly what it did before.

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
| `fallback` | array of strings | none | Optional. Aliases to try, in order, when this model fails. See below. |

### Fallback chain

A model that is rate limited, overloaded or unknown to its provider would otherwise end the send with an error. With `fallback`, the loop sends the same request to the next alias instead:

```toml
[models.main]
provider = "anthropic"
model = "claude-sonnet-5-5"
fallback = ["local"]

[models.local]
provider = "openai_compat"
model = "qwen-coder"
```

A fallback is used when the provider's own retries are spent and the call still fails with a rate limit, a server error or overload (HTTP 429, 5xx, 529), a connection failure that outlasts the retries, or an unknown-model error (an Anthropic `not_found_error`, an OpenAI-compatible 404 whose message names a model, or the code `model_not_found`; a bare 404 is treated as a wrong base URL and ends the send). Other errors, such as a bad key or a refused request, end the send as before. A fallback happens only before the model has said anything: once text, reasoning or a tool call has started to stream, a failure ends the send, so no half answer is thrown away. The send stays on the fallback until it ends, and the next send starts on the first model again. When the chain is used up the send ends with the last error.

Each switch is stored once as a `model_fallback` event that names both aliases and gives the reason, and the terminal UI shows it as a notice. Tokens from every model count against the same budget. The chain belongs to the alias; a session finds it by the provider and model it runs, using the first alias, by name, that names them and has a `fallback` list. Sub-agents started with `task` do not use fallbacks.

The aliases in a chain, and the alias that lists them, must be on `anthropic`, `openai_compat` or `faux`. A chain that names a missing alias, repeats an alias, lists the alias itself or names `claude_code` or `codex` is refused when the file loads. `coda doctor` warns for a fallback whose provider is not enabled; a session leaves such a fallback out and logs why.

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

## `[loop_guard]`

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `warn_after` | integer | `3` | Times the model must repeat one tool call, with the same arguments and the same result, before it is warned. Two calls taking turns, with the same results each round, count by rounds. The warning is added to the result of that call and names what repeated. At least 2. |
| `stop_after` | integer | `4` | Times it must repeat before the turn ends with the stop condition `stalled` (exit code 5 for `coda run`). Must be above `warn_after`. |

A different argument or a changed result starts the count again, so a poll whose output changes never trips the guard. The count also starts again with each new message you send. Identical calls in one reply count one by one. The same thresholds apply to sub-agents.

## `[review]`

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `max_findings` | integer | `5` | Most findings one review run reports. When the model returns more, the most severe are kept and the rest are counted in the run details. Findings about security are never cut, so a review can report more than this. Must not be zero. |
| `diff_budget_chars` | integer | `120000` | Most characters of diff text one review sends to the model. The diff is packed to fit (one part, when the review is split): binary, vendored and ignored files and deletion-only hunks are dropped, files of the repository's main language go first and smaller files before larger ones, and each hunk carries three lines of context and its new-side line numbers. Packing ends at the first file the budget cuts: that file keeps the hunks that fit whole, then the lines of the next hunk that fit (cut between lines, so one very large hunk still gives the model its first lines), and every later file is left out. A file where not even its header and the lines up to its first added line fit is left out too, and so are the files after it. A single line longer than the room that is left is not cut. The review's coverage list names every file as reviewed, clipped or left out. The `coda review` command applies it, and a change that does not fit is split into parts (see `max_chunks`). Must not be zero. |
| `max_chunks` | integer | `4` | Most parts one review splits a change into. When the packed diff does not fit `diff_budget_chars`, the files the budget cut go to a next part, each part is reviewed by its own run, and the results are merged. Files past the last part are listed as left out. Must not be zero. |
| `chunk_concurrency` | integer | `3` | Most parts reviewed at the same time. Must not be zero. |
| `verify` | boolean | `true` | Whether a `verifier` agent checks the findings before they are shown (see `coda review`). It runs once per review and only when there is a finding. |
| `verify_threshold` | integer | `4` | Findings the verifier scores below this, on its scale of 0 to 10, are dropped. A score of 0 is always dropped. At most 10. |
| `guidance_lines` | integer | `500` | Most lines of the repository's own guidance that go in the reviewer's prompt as project rules: `AGENTS.md`, `CLAUDE.md` and the first 20 files of `docs/adr/*.md` by name, read from the default branch with `git show` in that order. A cut ends the text with `[guidance cut: N more lines are not shown]`, and a line is cut at 1000 characters. Zero leaves the guidance out. See `coda review`. |
| `linked_repos` | array of strings | `[]` | Repositories (`org/repo`) whose issues a pull request may link to and have read into the review. Issues of the pull request's own repository are always read. Only the global config sets it, never `.coda/review.toml`. See `coda review`. |
| `extra_instructions` | string | `""` | Instructions added to every review's prompt after the project rules. A `.coda/review.toml` on the default branch can set it too, and the two are added together, the config's first. |

## `coda review`

`coda review [<target>]` reads a change, has the built-in `reviewer` agent look at it and prints the findings. It posts nothing unless you pass `--post`, and edits no file of the repository. The target is a pull request number or URL (read with the `gh` command, which uses your own `gh` login), a branch (against the default branch), a range such as `main...topic` (from the merge base; `main..` ends at `HEAD`), or nothing for the current branch against the default branch. All digits, or a URL with `/pull/`, is a pull request, so a branch named `42` cannot be reviewed by name; give it as a range. The default branch is where `origin/HEAD` points, else a local `main`, else a local `master`.

| Flag | Meaning |
| --- | --- |
| `coda review --staged` | Review the staged changes instead of a target. |
| `coda review --instructions <text>` | Extra instructions for the reviewer, for this run only, from you. They come after `[review] extra_instructions`. |
| `coda review --json` | Print the review as JSON, in the schema of the `coda-review` crate, instead of markdown. |
| `coda review --backend <name>` | `anthropic`, `claude-code` or `codex`. Without the flag, the backend `coda run` would use: the faux provider if it is enabled, else Claude Code, else Codex, else the Anthropic API. |
| `coda review --cwd <dir>` | The directory to review in. |
| `coda review --force` | Review a pull request although a `[review.skip]` rule matches. |
| `coda review --post <pr>` | After the review, show a preview and, when you answer `y`, post it to the pull request as your own `gh` login. See below. |
| `coda review --post --yes <pr>` | Post without asking. Needed when standard input is not a terminal; without it `--post` refuses to run, before any model call. |

The reviewer's prompt holds, in this order, the reply format, the project rules (`[review] guidance_lines`), `[review] extra_instructions` from the config and from the default branch's `.coda/review.toml`, the instructions you give for this run, and the diff. Rules and instructions are read from the default branch only (a branch ref such as `origin/HEAD`, else a local `main` or `master`), never from the working tree or the branch under review, so a pull request cannot change what the prompt holds (the reviewer can still read the files of the checkout, and is told to take no rules from them); a repository with no default branch gets none, and `coda review` says so on standard error. The line cap counts the lines of the files, not the line above each file or the closing marker, and the text is not part of `diff_budget_chars`. The diff is marked in the prompt and in the `reviewer` agent's instructions as untrusted data written by its author, and anything in it that looks like rules or orders is to be ignored. The diff is packed to `[review] diff_budget_chars` and the ignore rules apply (see above). On `claude-code` and `codex` the daemon runs the `reviewer` agent directly with the daemon request `agent.run`: the agent's instructions and the packed diff go in the prompt, so no file is written and no other model reads the diff first. The session is in plan mode: every permission request is denied, Claude Code is started with its editing and shell tools (`Edit`, `Write`, `MultiEdit`, `NotebookEdit`, `Bash`) banned, which beats any allow rule in your own settings, and Codex runs in its read-only sandbox even if `providers.codex.sandbox` is wider. The agent can read files and cannot change them. Claude Code and Codex narrate as they work, so the review is the last JSON object in what they wrote that has a `summary` and a `findings` key. On the Anthropic API the reviewer is a sub-agent that a session starts through the `task` tool. The diff is too large to ask that session's model to copy into a tool call, so the packed text is written to `.coda/review/input-<pid>.txt` in the working directory and the reviewer reads it with its `read` tool; the file (one per part, the later ones numbered `input-<pid>-<n>.txt`, and `verify-<pid>.txt` for the verifier) is removed when the command ends, and the reviewer there has only read-only tools: no `bash`, `edit` or `task`. Each finding must point at lines the diff shows; others are dropped and counted. If the answer is not in the review format, the reviewer is asked once more. A change that does not fit `[review] diff_budget_chars` is split by file into at most `[review] max_chunks` parts. Each part is a run of its own, at most `[review] chunk_concurrency` at a time, and the reviewer is told which part of how many it sees. The parts are merged without losing anything a part reported: the findings of all parts are kept, one repeated with the same file, lines and text is kept once with the worst severity and the lowest confidence, the risk and the effort are the highest of the parts, and the finding cap (`max_findings`) cuts the least serious findings first but never one about security. A part whose run fails is tried once more. If it fails again the review is partial: it opens with a `Partial review` banner, keeps the findings of the other parts, lists the files of the failed part under Left out, and the JSON has `run.partial` set to true. A review where every part fails is a failure like any other. `run.chunks` is the number of parts.

After the parts are merged, when there is at least one finding and `[review] verify` is on, the built-in `verifier` agent gets all findings and the lines of the diff around each, and gives each a score from 0 to 10 and a one-line reason. It runs on the other of Claude Code and Codex when that one is enabled in the config (coda has no check of whether you are logged in, so "enabled" stands for it), otherwise on the backend of the review, with the same read-only rules as the reviewer. A finding scored 0, or below `verify_threshold`, is dropped and counted in `run.verifier_dropped`; the rest are ranked by score, highest first, with their score and reason shown, and numbered again. If the verifier fails, answers in the wrong shape or leaves a finding out, coda keeps that finding, marks it `unverified` and gives it no score. `run.verifier` names the backend that was asked. The verifier is not asked again after a bad answer. Cancelling with Ctrl-C ends the review, also while the verifier runs. `/review` in the terminal UI does not run the verifier. A review is skipped (exit 0, one line printed) when a skip rule matches a pull request, and when the change has no lines to look at. A failure exits 70 with the reason on standard error and nothing on standard output. In the terminal UI, `/review [target]` asks the session's model to hand the change to the reviewer.

`coda review --post` needs a pull request target (a number or URL; a branch or range has nowhere to post to) and cannot be combined with `--staged`. It runs only `gh`, so it acts as your own `gh` login and reads no token. The findings that sit on a line of the pull request's diff become the inline comments of one pending review, created on the pull request's head commit with no approve or request-changes event, so the inline comments are visible to others only after you submit it on GitHub. The summary comment is an ordinary comment and is visible at once. The other findings, with the risk, the effort and the summary, go in one summary comment that starts with a hidden marker. Each inline comment ends with a hidden fingerprint of its finding (file, category and headline, ignoring case and punctuation). A rerun rewrites your earlier summary comment instead of adding one (it holds this run's findings only, with the partial banner and the clipped and left out files), and stops if the pull request got a new commit since the review began, and skips a finding whose fingerprint is already in a comment of yours, submitted or pending. GitHub keeps one pending review per user, so when you have one and there are new comments, `--post` stops and asks you to submit or discard it first. The preview and the question go to standard error; the review itself still goes to standard output.

For a pull request target, `coda review` also reads the issues it links to. It looks for `#12`, `owner/repo#12`, a full issue URL and a branch name that starts with the number (`123-fix-bug` or `fix/123-bug`) in the title, the body and the source branch, and reads up to three with `gh issue view --json`: the title, the labels, the body (cut at 4000 characters) and its task list items. They go in the prompt as untrusted data, and the reviewer also gives a verdict for each requirement of the issues that the change should meet: fully, partially or not compliant, with the requirement quoted from the issue. A verdict that names an issue that was not read, or whose quote is not in that issue's title or body, is dropped. The markdown shows them under `Linked issues` and the JSON has a `compliance` list. A reference to a repository other than the pull request's own is read only when it is in `[review] linked_repos`. An issue that cannot be read (not found, no access) is skipped with a note under `Linked issues` and in `run.skipped_issues`, and gets no verdict. With no reference the section is left out. When the change is split into parts, a requirement keeps the best verdict any part gave it, since a part sees only some of the files. A branch, a range or `--staged` has no pull request and so no linked issues.

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

Review rules are read from the global `config.toml` and from `.coda/review.toml` on the repository's default branch (the library takes a `refs/heads/` or `refs/remotes/` ref such as `refs/remotes/origin/main`, so a tag with the branch's name cannot stand in for it), with the same `[review.ignore]` and `[review.skip]` tables, and `[review] extra_instructions`. Lists from both add up, `draft` and `bots` are on when either turns them on, and the built-in ignore list stays on only if both keep it. A `.coda/review.toml` on the pull request's own branch is never read, so a pull request cannot switch off its own review.

## `[[hooks]]`

A hook is a program coda runs at a point in a model-API session (`anthropic`, `openai_compat`, `faux`). Hooks run in coda's own tool path. Claude Code and Codex run their own tools and coda only sees the calls it is asked to approve through its permission broker, so no coda hook runs for those sessions. When the config has hooks and a session runs on one of those backends, the terminal UI shows a notice when the session starts and `coda run` prints one line on standard error, so a guard is never assumed to be active. Hooks come from this file only. A project file or a definition file cannot add one.

An entry is a table in an array, so it has no single default. The keys:

| Key | Type | Meaning |
| --- | --- | --- |
| `event` | string | Required. `"session_start"` (before the first send of each turn loop, which can happen more than once in a conversation, see below), `"pre_tool"` (after a tool call is allowed, before the tool runs), `"post_tool"` (after the tool ran, or crashed) or `"end_turn"` (when the model ends its reply without tool calls, see below). |
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
| `end_turn` | `text` (the model's reply, empty when it said nothing, cut at 64 KiB with `text_truncated` true when longer), `continues` (how many times a hook already sent the model back in this send) |

A hook gets a call whole up to 256 KiB of arguments (as JSON text) and 64 KiB of result, counted before the text is escaped inside the payload. A result above 16 KiB reaches the model compacted, so a guard sees every byte the model sees. Past those limits a blocking hook is not run: it fails closed and the call is stopped with a reason that names the part and its limit, for example `the result is too large for the hook to check (over 65536 bytes)`. A hook that is not blocking is run on a cut payload instead: `arguments` becomes the first 256 KiB as text and `arguments_truncated` is true, or `result` is cut and `result_truncated` is true.

A hook fails when it cannot start, exits with a status other than 0, or times out. What a failure does:

- A failing blocking `session_start` hook stops the turn before the model is called, and the next send tries it again. `coda run` prints the reason on standard error, prints nothing on standard output and exits 5 (the stop condition is `hook_blocked`). The terminal UI shows the reason as a notice. The reason is never sent to the model.
- A failing blocking `pre_tool` hook stops the tool. The model gets an error naming the hook and the reason.
- A failing blocking `post_tool` hook replaces the tool's result with that error. This holds for a tool that crashed too, and for a turn that is cancelled while the hook runs or just as the tool returns: with a blocking `post_tool` hook set, the model gets an error saying the result is withheld, not the unchecked result. A result withheld this way, or replaced by a hook's error, leaves no stored original for `retrieve` to return. With only hooks that are not blocking, a cancelled turn keeps the result.
- A blocking `end_turn` hook that exits with status 2 sends the model back to work: the last 1 KiB it wrote on standard error (or a fixed line, when it wrote nothing) is added to the conversation as a user message and the loop calls the model again. The reason is stored as a `guard_continue` event and the terminal UI shows it as a notice. Each forced continue is a model call, so it counts against the token budget and `max_turns`. After `max_guard_continues` forced continues in one send, a hook that asks again ends the turn with the stop condition `guard_limit` (exit code 5 for `coda run`), and a `guard_limit` event is stored. Any other end of an `end_turn` hook, a crash, a timeout, a start failure or another exit status, lets the turn end as if the hook passed, and is logged. An `end_turn` hook that is not blocking never sends the model back. Sub-agents run no `end_turn` hook: it guards the session's own answer.
- A failing hook that is not blocking is logged as `hook failed, continuing: event=<event> hook=hooks[<index>] (<program>) reason=<why>`.

A hook is named by its place in the `[[hooks]]` list, counting from 0, and its program, for example `hooks[2] (/bin/sh)`, so two shell hooks are told apart. The reason is how the hook failed (`exited with status 3`, `timed out after 5000 ms`, `could not start: ...`) and, when it wrote any, the last 1 KiB of its standard error after a colon. It is folded onto one line and has no control or format characters, so a hook cannot forge a log line, move the cursor of a terminal or hide text with a zero width or right-to-left character.

`session_start` runs once for each turn loop of a session, on its first send, not once per send. The daemon keeps a session's loop between sends, so the hook runs again whenever the daemon builds the loop again from the stored history: after a daemon restart, after `/model` switches the session's model, after a change to the skills, the agents or the trust decision rebuilds the loop, and after an idle loop is dropped to make room or when the session is closed. `resumed` is true when a `session_start` hook already passed for the session. The daemon stores that as a `session_started` event when the hook passes, so a blocked first send, which leaves messages in the history, is still a first start when it is tried again, and a conversation that was started before any `session_start` hook was set reports `resumed` false the first time.

A sub-agent started by the `task` tool (see [agents.md](agents.md#dispatch-the-task-tool)) runs the `pre_tool` and `post_tool` hooks for its own tool calls, so a blocking hook stops a sub-agent's call as it stops the session's, and a sub-agent is no way around one. It runs no `session_start` hook: that event marks the start of the session, not of a sub-agent. The session's `task` call runs no hook, because dispatching asks no permission and does nothing on the machine; the sub-agent's tool calls are what the hooks see. The `session` in a sub-agent's payload is the id of the session that started it.

A hook that times out or whose turn is cancelled is killed together with everything it started (its process group). A hook that exits on its own is not followed: coda signals nothing after the hook's own process has ended, because its process group id may belong to someone else by then, so a process the hook left running keeps running. Only the end of the hook's standard error is read, and only for a failed hook, for at most half a second after it exits; what was read by then is the reason, even when a process the hook left behind still holds the pipe open.

A key the schema does not know in a hook entry is an error, so a misspelled `blocking` never turns a guard into a log line.

## `[checks.<extension>]`

A check is a command coda runs after the model changes a file, so the model sees what it broke in the same tool result and can fix it at once. It runs in model-API sessions (`anthropic`, `openai_compat`, `faux`) after a successful `edit`, `write` or `multi_edit`, for files with the extension named in the table, for example `[checks.rs]`. The extension has no dot and is matched without regard to case. Nothing runs for an extension with no table, and there are no checks unless you add them. Checks come from this file only. A project file or a definition file cannot add one.

| Key | Type | Meaning |
|---|---|---|
| `command` | array of strings | Required. The program and its arguments, with the same rules as a hook's `command`: no shell, an absolute path or a bare name found on `PATH`, and the same environment as a hook. It runs in the session's directory. |
| `timeout_ms` | integer | Longest run time, from 1 to 300000. Default 60000. |

How it behaves:

- It runs once after the last change in a model reply, not after each change. If a reply makes two edits, the model gets the check result in the result of the last one. Two extensions with the same command run it once.
- A check that passes adds one line, `check passed (<program>)`. One that fails adds `check failed (<program>, exited with status N)` and the end of what it wrote to standard output and error, as plain text (colour codes removed). Only the last 32 KiB of output is read, and a long log is shortened to its start, its errors and its end. The result is added after the tool's `post_tool` hooks have run, so a `post_tool` hook does not see the check text: the command is one you chose in this file.
- A check that times out adds `check timed out after N ms (<program>); the change was applied`, and one that cannot start adds `check could not run`. The change stays either way. A check never turns a successful edit into an error.
- It is killed with its whole process group at its timeout or when the turn is cancelled. A change made when the reply's last edit failed is still checked, and the result is added to the failed edit's result.
- A sub-agent started by the `task` tool runs the checks for its own changes.

## Example

This file uses every key. It is checked against the parser when the tests run.

```toml coda-config
max_turns = 20
token_cap = 500000

[loop_guard]
warn_after = 3
stop_after = 5

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
max_chunks = 4
chunk_concurrency = 3
verify = true
verify_threshold = 4
guidance_lines = 500
extra_instructions = "Check that every lock is released on the error paths."

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

[checks.rs]
command = ["cargo", "check"]
timeout_ms = 120000
```
