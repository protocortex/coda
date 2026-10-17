# coda

A coding agent, harness and chat interface, written in Rust.

Status: early development. The previous TypeScript implementation lives in `protocortex/coda-ts`.

License: AGPL-3.0-only. See `LICENSE`.

## Build

```sh
cargo build --workspace
```

## Permissions

The permission broker decides tool requests in three modes: `prompt`, `accept-edits` and `deny-all`. Always-allow grants are stored per repository and match the exact tool, working directory and command text.

For delegated sessions the broker is not authoritative for tools that Claude Code auto-allows through its own settings.
