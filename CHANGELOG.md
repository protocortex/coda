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
- Agent, skill and command definition files and a loader for them. Project definitions are listed as pending and not used yet. See [docs/agents.md](docs/agents.md).

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
