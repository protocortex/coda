# Self-hosting acceptance

Coda proves itself by doing a small piece of its own development: add a
`coda version --json` subcommand that prints the crate name and version as JSON,
with a test. An attempt passes when `scripts/selfhost-verify.sh` accepts it.

## What is checked

The verifier (`crates/coda-selfhost`) accepts an attempt only when all hold:

1. The diff touches `crates/coda-cli/src/main.rs`.
2. The diff adds a test (`#[test]` or `#[tokio::test]`).
3. `cargo test` exits 0.
4. `coda version --json` prints a JSON object whose `name` and `version` equal
   the `coda-cli` package metadata from `cargo metadata`.

Exit codes: 0 accepted; 1 rejected, including a failing `cargo test` or a
cargo step that outlives its time limit (a retry may help); 2 usage error; 3 the
verifier itself failed (git or file errors, cargo missing), which a retry loop
should not count as a failed attempt. The verifier's cargo runs the code the
attempt wrote, so it does not inherit `ANTHROPIC_API_KEY` (for `live`, the
variable named by `--api-key-env`, or `ANTHROPIC_API_KEY` with the
`claude-code` or `codex` backend).

## Regression run in CI

`crates/coda-cli/tests/selfhost.rs` runs on every `cargo nextest run`. It copies
the repository to a temporary directory, replays a scripted faux provider
through the real `coda run` and daemon, and asserts the exact resulting
`main.rs` and `tests/version.rs` against the pinned edits in
`crates/coda-selfhost/src/task.rs`. It also asserts that an edit to `Cargo.toml`
is denied (`coda run` is headless and denies every prompt) and that the verifier
rejects no diff, no test added, and wrong JSON.

The verifier's cargo builds into `target/selfhost` (or
`$CARGO_TARGET_DIR/selfhost`), so the `coda` binary other tests run is not
overwritten. It uses two build jobs and drops the outer `RUSTFLAGS` and
coverage variables, so a `cargo llvm-cov` run does not instrument it. The two
building tests run alone (`.config/nextest.toml`).

## Live run

Status: not run yet. OI-1 (whether headless use is allowed) was confirmed by the
user on 2026-10-21, and OI-2 (allow-path provider fixtures) is closed by the live
transcripts under `crates/coda-testkit/fixtures/`; see
[live-recording.md](live-recording.md). No transcript exists under
`docs/selfhost/`, and none may be written by hand.

```sh
export ANTHROPIC_API_KEY=...        # or name another variable with --api-key-env
scripts/selfhost.sh                 # attempts only, nothing is written
scripts/selfhost.sh --write-transcript
scripts/selfhost.sh --backend claude-code   # no key, uses your Claude Code login
scripts/selfhost.sh --backend codex         # no key, uses your Codex login
```

### Running with your installed Claude Code or Codex

`--backend codex` is the same idea with the `codex` binary: `[providers.codex]`
with `enabled = true`, `sandbox = "workspace-write"` and
`allow_wider_sandbox = true` (the default sandbox is read-only, which cannot
make the edit), under your Codex login in `~/.codex`. `OPENAI_API_KEY` is
withheld along with `ANTHROPIC_API_KEY`. `CODEX_HOME` is not forwarded, so the
default `~/.codex` is used. The adapter keeps approvals on request and routes
them to coda's broker; the copy is a git repository, which `codex exec`
requires. Codex also runs its own tool loop.

`--backend claude-code` writes `[providers.claude_code]` with `enabled = true`
and so runs the delegated backend: the `claude` binary
found on `PATH`, signed in as you, under your subscription. No API key is needed and the key check is
skipped. The run is headless use of Claude Code, which the user confirmed
(OI-1); the account risk notice in
[README](../README.md#account-risk-notice) applies to it.

- `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` are removed from the environment of `coda run` and of the
  verifier's cargo, so the run cannot silently bill the API. The daemon also
  starts `claude` with an allowlisted environment (`ENV_ALLOWLIST` in
  `crates/coda-providers/src/claude_session.rs`) that includes `HOME`, which
  the CLI needs to find its login, and excludes every key.
- The scrubber still runs over every transcript and printed line.
- Claude Code runs its own tool loop. This run therefore exercises coda's
  daemon, permission broker, supervisor and grants, not coda's own tool
  implementations (those are covered by the Anthropic backend run).

`scripts/selfhost.sh` only builds and calls `coda-selfhost live`. The command:

- refuses to start, before it copies or spawns anything, if the key variable is
  not set. It never reads the key itself: the generated `config.toml` holds the
  variable's name (`api_key_env`), and `coda` reads the value.
- runs up to 3 attempts (`--attempts`, 1 to 3), each in a fresh temporary copy
  with a fresh `CODA_HOME`, and stops at the first one the verifier accepts.
- uses, by default, the Anthropic provider with model `claude-sonnet-5-5` (`MODEL` in
  `crates/coda-selfhost/src/attempt.rs`), mode `accept-edits`, 30 turns and a
  400000 token cap. In `accept-edits` mode edits to any path inside the copy
  are allowed without a grant, except execution-affecting paths (`Cargo.toml`,
  `build.rs`, `rust-toolchain.toml`, `.cargo/config.toml`, `.git/hooks`,
  `.github/workflows` and `scripts`, see `is_execution_affecting` in
  `crates/coda-providers/src/policy.rs`), which
  need a prompt that a headless run denies; a grant never overrides that. The
  grants that matter are the exact commands `cargo test` and `cargo build`
  (`[permissions] grants`), the only shell commands that are approved; the
  `edit:` grant for `main.rs` in the config changes nothing in this mode. Every
  other prompt is denied. Paths outside the copy are always denied.
- writes `docs/selfhost/attempt-N.txt` only with `--write-transcript`, and
  refuses to start if `docs/selfhost` is a symlink or already holds an
  `attempt-*` file, so a stale transcript never survives a run. Each file is
  written to a temporary name and renamed into place. Each transcript has environment values (including the key), `sk-` style keys,
  absolute paths, email addresses, UUIDs and long opaque ids replaced by
  `<env:NAME>`, `<key>`, `<path>`, `<email>`, numbered stand-in UUIDs and
  `<opaque>`. Read it before committing it.

Everything printed to the terminal has the same scrubbing applied, including
rejection reasons and errors.

A `coda run` or cargo step that outlives its limit rejects that attempt and the
next attempt runs.

Ctrl-C (SIGINT) or SIGTERM kills the running `coda` process group, stops its
daemon, removes the temporary root and exits 130.

Exit codes: 0 an attempt passed; 1 every attempt was rejected (timeouts
included); 2 usage error; 3 the run itself failed (no key variable, `coda`
missing, existing transcripts, git errors); 130 interrupted. Code 3 stops the
run at once instead of burning another attempt.

`coda-selfhost scrub FILE` prints any text file with the same scrubbing applied.
CI covers the attempt loop, the scrubbing and the flag with the faux provider
and a stand-in `coda`; none of it touches the network.
