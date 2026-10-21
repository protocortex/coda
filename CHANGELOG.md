# Changelog

All notable changes to Coda are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- A release workflow that builds, checksums and attests the `coda` archives for Linux and macOS, and an `install.sh` that verifies the checksum before installing.

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
