# coda

A coding agent, harness and chat interface, written in Rust.

Status: early development. The previous TypeScript implementation lives in `protocortex/coda-ts`.

License: AGPL-3.0-only. See `LICENSE`.

## Account risk notice

Coda can drive the `claude` and `codex` programs you have installed. If you sign in to them with a subscription, Coda uses that subscription on your behalf.

Coda has not verified that the Anthropic terms allow this use of the `claude` binary. Coda has not verified that the OpenAI terms allow this use of the `codex` binary. You must check the terms for your account yourself. A mistake can put your account at risk.

The claude_code and codex backends stay disabled until you enable them. Set `enabled = true` under `[providers.claude_code]` or `[providers.codex]` only after you have read the terms. The `anthropic`, `openai_compat` and `faux` providers use an API key or a script, not a subscription.

## Install

Install a release build into `~/.local/bin`. The script downloads the archive for your platform (Linux or macOS, x86_64 or arm64), checks it against the release `SHA256SUMS`, and installs it. It never uses `sudo`. Set `CODA_INSTALL_DIR` to install elsewhere and `CODA_VERSION` to pick another release.

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/protocortex/coda/v0.1.0/scripts/install.sh | sh
```

Or build the same tag from source:

```sh
cargo install --locked --git https://github.com/protocortex/coda --tag v0.1.0 coda-cli
```

`SHA256SUMS` comes from the same release as the archive, so it detects a corrupted download, not tampering. To tie an archive to this repository's release workflow, check its attestation with the GitHub CLI:

```sh
gh attestation verify coda-0.1.0-x86_64-unknown-linux-gnu.tar.gz -R protocortex/coda
```

## Build

You need a stable Rust toolchain. The repository pins one in `rust-toolchain.toml`.

```sh
cargo build --workspace
```

The binary is `target/debug/coda`. Put it on your `PATH` for the commands below.

## First run

Write a starter config, enable one provider in it, then restart the daemon and open the terminal UI.

```sh
coda init
$EDITOR "${CODA_HOME:-$HOME/.config/coda}/config.toml"
coda daemon restart
coda
```

`coda init` writes `config.toml` with every provider section commented out. It stops with an error if the file already exists, so it does not overwrite your settings. Plain `coda` and `coda tui` start the daemon if none is running and open the terminal UI. With no provider enabled, the UI tells you to run `coda init` and edit the config.

## Quickstart with the faux provider

The faux provider replays a script you write. It needs no network and no account, so it is the safest way to try Coda.

Use a new, short `CODA_HOME`. The daemon socket lives inside it, and macOS limits socket paths to 104 bytes.

```sh
export CODA_HOME="$(mktemp -d /tmp/coda.XXXXXX)"
```

Write a script with one turn, and a config that enables the faux provider.

```sh coda-test
mkdir -p "$CODA_HOME"
cat > "$CODA_HOME/script.json" <<EOF
[{"events": [
  {"type": "text_delta", "text": "hello from faux"},
  {"type": "usage", "input_tokens": 11, "output_tokens": 7},
  {"type": "stop", "reason": "end_turn"}
]}]
EOF
cat > "$CODA_HOME/config.toml" <<EOF
[providers.faux]
enabled = true
script = "$CODA_HOME/script.json"
EOF
```

Run a prompt. `coda run` starts a daemon if none is running. With the faux provider enabled it answers from the script.

```sh coda-test
coda run --prompt "say hello" --cwd "$PWD"
```

```text coda-output
hello from faux
```

Check the daemon and the token counts.

```sh coda-test
coda ping
coda usage
```

```text coda-output
pong
input tokens: 11
output tokens: 7
cache read tokens: 0
cache write tokens: 0
saved bytes: 0
```

Run the health check. It exits 0 when every check passes.

```sh coda-test
coda doctor --json | grep -q '"healthy":true'
```

A grant lets one exact command run without a prompt. It applies to the git repository you name. This block makes a new repository in a new directory, so it leaves your own working directory alone.

```sh coda-test
cd "$(mktemp -d)"
git init -q .
coda grant add --repo . "cargo test"
```

Stop the daemon when you are done.

```sh coda-test
kill "$(coda status | sed -n 's/^pid: //p')"
```

## Commands

| Command | What it does |
| --- | --- |
| `coda` | On a terminal, start the daemon if needed and open the terminal UI. Without a terminal it starts the daemon and prints its pid. |
| `coda tui` | Open the terminal UI, starting the daemon if needed. It fails with a message when there is no terminal. |
| `coda init` | Write a starter `config.toml` with every provider commented out. It does not overwrite an existing file. |
| `coda daemon` | Run the daemon in the foreground. `coda daemon restart` replaces a running one. |
| `coda status` | Show the daemon pid, version and uptime. It fails if no daemon runs. |
| `coda ping` | Print `pong` if a daemon answers. It fails if no daemon runs. |
| `coda run` | Run one prompt to completion. Flags: `--prompt`, `--cwd`, `--mode`, `--max-turns`, `--token-cap`. Without `--prompt` it reads the prompt from standard input. |
| `coda grant add` | Allow one command in a repository permanently. Flags: `--repo`, `--cwd`, `--tool`. |
| `coda usage` | Show token usage and savings. `--session` limits it to one session. It asks a running daemon, or reads the database when none runs. |
| `coda doctor` | Check the socket, lock, orphaned backends, database, schema, database release, config and backends. `--repair` removes a stale socket and kills orphaned backends. `--json` prints JSON. |

`coda run`, `coda grant add`, `coda tui` and bare `coda` start a daemon if none is running. `coda daemon restart` starts one too. `coda status`, `coda ping`, `coda usage` and `coda doctor` do not start one.

`coda run` picks its backend from the config. It uses the faux provider if `[providers.faux]` is enabled, and the anthropic provider otherwise. It does not pick `openai_compat`, `claude_code` or `codex`.

Every command accepts `--force`, but it only matters in two cases. A command that starts a daemon (`coda run`, `coda grant add`, `coda tui`, bare `coda`) asks before it replaces a busy daemon that speaks another protocol version, and `--force` skips that question. `coda daemon restart` refuses to replace a busy daemon unless you pass `--force`.

## Exit codes

`coda run`:

| Code | Meaning |
| --- | --- |
| 0 | The turn ended. |
| 1 | The turn limit was reached. |
| 2 | The token budget was used up. |
| 3 | The context window is full. |
| 4 | The run was cancelled. |
| 5 | Any other stop, or a failed turn. This includes a conversation that could not be compacted. |
| 64 | Bad usage. |
| 70 | The run itself failed. |

`coda doctor`: 0 healthy, 20 at least one check failed, 21 only warnings, 64 bad usage, 70 the check itself failed.

`coda agents lint`: 0 no problems, 22 at least one problem, 64 bad usage, 70 the lint itself failed.

## Configuration

Settings live in `config.toml` under `CODA_HOME`. Without `CODA_HOME`, Coda uses `$HOME/.config/coda`. See [docs/config.md](docs/config.md) for every key, its default and its checks.

Backends: `claude_code`, `codex`, `anthropic`, `openai_compat` and `faux`. Each is a `[providers.<name>]` section.

## Context and compaction

Each model has a context window. Coda knows it for the models it names (see [docs/config.md](docs/config.md)) and you can set it with `context_window` on a `[models.<alias>]` section. When a request reaches 80% of a known window, or the provider reports that a request is too large, Coda asks the same model to summarize the older messages, keeps the most recent ones as they are, and retries the request once. A tool call and its results are never split. The summary is stored as a `compaction` event, with its text kept apart from the event log, and a resumed session replays it. The summary is sent as a user message that says it is background data, never as a system prompt. If the summary cannot be made, the run stops as `compaction_failed` (exit code 5). If the request is still too large after compaction, it stops as `context_overflow` (exit code 3). A model with no known window is compacted only after an overflow.

## Permissions

The permission broker decides tool requests in three modes: `prompt`, `accept-edits` and `deny-all`. Always-allow grants are stored per repository and match the exact tool, working directory and command text.

For delegated sessions the broker is not authoritative for tools that Claude Code auto-allows through its own settings.

## Upgrades and the database

Every database migration is tied to the release that introduced it. A database written by a newer release is refused, not guessed at. Move to a newer release, not back to an older one.

## More

Self-hosting acceptance: [docs/selfhost.md](docs/selfhost.md).
