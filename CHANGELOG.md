# Changelog

All notable changes to Coda are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- A release workflow that builds, checksums and attests the `coda` archives for Linux and macOS, and an `install.sh` that verifies the checksum before installing.
- A `session.close` request that stops a session's idle Codex server or model conversation and keeps its stored events. `coda run` calls it when the turn ends, and the terminal UI calls it for a session it created when the user quits or switches away.
- A `cause` on `turn_interrupted` events (`cancelled`, `idle_timeout`, `backend_lost`, `daemon_restarted`, `daemon_stopping`). `coda run` exits 4 for a turn cancelled from another client and 5 for the other causes, naming the cause.
- `daemon.status` reports `any_provider_enabled`, so the terminal UI shows the setup guidance when a daemon that started before `coda init` has no provider.
- Agent, skill and command definition files with a loader, `coda agents list` and `coda agents trust`. Project definitions load only after the repository is trusted, and trust ends when they change. See [docs/agents.md](docs/agents.md).
- `[[hooks]]` in the user's `config.toml`: programs coda runs at `session_start`, `pre_tool` and `post_tool` in a model-API session. A hook gets a JSON payload on standard input and a minimal environment without API keys. It is killed with its process group at its timeout, and only a `blocking` hook can stop the action it guards. See [docs/config.md](docs/config.md#hooks).
- `coda agents lint` checks definition files for unknown tiers, efforts and tools, missing usage guidance and model ids, and a `[routing]` config table maps agent tiers to models.
- A `task` tool that runs a named agent as a child conversation with only its tools, its tier's model and its own permission prompts, which reach the parent's client with the agent named in the `agent` field of `permission.request`, and the terminal UI draws it apart from the command. See [docs/agents.md](docs/agents.md).
- A `task` result reports a status (`completed`, `failed` or `cancelled`), a summary and findings classified as blocking, non-blocking or suggestion. The agent's words are marked as data and capped at 16 KiB with the findings kept whole, blocking first, and a malformed report is rejected. A `task` call with a blank prompt is refused.
- Built-in agents `explore`, `reviewer`, `implementer` and `tester`, available to the `task` tool with no files. A user file of the same name replaces one. A trusted project file of the same name can replace its prompt and narrow its tools, but keeps its tier and effort; `coda agents lint` reports a tier, effort or tool the project file asked for and did not get.
- Command files are slash commands in the terminal UI (`/name args`, listed by the new `/help`) and run headless with `coda run --command <name> --args <text>`. Without `--args`, piped standard input (up to 256 KiB) is the arguments of a command that has a `$ARGUMENTS`; an unknown command fails before it is read, and a command without `$ARGUMENTS` never reads it. `$ARGUMENTS` in the prompt template is replaced by the arguments as plain text, and a prompt that is blank after that is not sent. Only your own and trusted project commands run, and a built-in TUI command wins a name clash in the TUI. The terminal UI reads trust once, when it starts. It says when definition files were skipped.
- Skills for model-API sessions. The model is offered the skills by name and description in the description of a new `skill` tool, and a skill's body enters the conversation only when the model calls the tool for it. Only your own skills and those of a trusted project are offered.

## [0.1.0] - Unreleased

First release.

### Added

- `coda` and `coda tui` open the terminal UI; without a terminal, bare `coda` prints the daemon status.
- `coda init` writes a starter `config.toml` and refuses to overwrite an existing one.
- The daemon, its Unix socket protocol and the commands `run`, `status`, `ping`, `grant`, `usage` and `doctor`.
- Providers `claude_code`, `codex`, `anthropic`, `openai_compat` and `faux`, all off until enabled in `config.toml`.
- Release-versioned database migrations: a database written by a newer release is refused.
- A `history_not_restored` event for a Codex thread that could not be given the earlier conversation.
- Context windows per model, with a `context_window` override, and compaction of the conversation when a request nears the window or overflows it.

[Unreleased]: https://github.com/protocortex/coda/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/protocortex/coda/releases/tag/v0.1.0
