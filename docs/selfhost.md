# Self-hosting acceptance

Coda proves itself by doing a small piece of its own development: add a
`coda version --json` subcommand that prints the crate name and version as JSON,
with a test. An attempt passes when `scripts/selfhost-verify.sh` accepts it.

## What is checked

The verifier (`crates/coda-selfhost`, called by `scripts/selfhost-verify.sh`)
accepts an attempt only when all of these hold:

1. The diff touches `crates/coda-cli/src/main.rs`.
2. The diff adds a test (`#[test]` or `#[tokio::test]`).
3. `cargo test` exits 0.
4. `coda version --json` prints a JSON object whose `name` and `version` equal
   the `coda-cli` package metadata from `cargo metadata`.

Exit codes: 0 accepted; 1 rejected, including a failing `cargo test` (a retry
may help); 2 usage error; 3 the verifier itself failed (git, cargo or file
errors), which a retry loop should not count as a failed attempt.

The logic is Rust with unit tests; the shell scripts only start it.

## Regression run in CI

`crates/coda-cli/tests/selfhost.rs` runs on every `cargo nextest run`. It copies
the repository to a temporary directory, replays a scripted faux provider
(`providers.faux.script`) through the real `coda run` and daemon, and asserts the
exact resulting content of `main.rs` and `tests/version.rs` against the pinned
edits in `crates/coda-selfhost/src/task.rs` applied by plain replacement. It also asserts that:

- an edit to `Cargo.toml` (an execution-affecting path) is denied at once, because
  `coda run` is headless and denies every permission prompt, so nothing changes and the verifier rejects the attempt;
- the verifier rejects the three negative cases: no diff, no test added, and
  wrong JSON.

The verifier's `cargo test` builds into `target/selfhost` (or
`$CARGO_TARGET_DIR/selfhost`), never into the workspace's own output directory,
so the `coda` binary other tests run is not overwritten. Delete that directory to
reclaim disk. Inside it, `CODA_SELFHOST_NESTED` is set so the regression test
does not start another run.

## Live run (parked)

Status: **not run**; no live transcript is committed and its runner
(`scripts/selfhost.sh`) is not part of this change. It waits on OI-1 (whether
the account's terms allow this headless use of the Anthropic API) and OI-2
(allow-path provider fixtures).

The plan: up to 3 attempts in fresh temporary copies with model
`claude-sonnet-5-5`, accept-edits mode, a turn cap, a token cap and the
pre-grants in `crates/coda-selfhost/src/task.rs`. An edit to an
execution-affecting path (`Cargo.toml`, `build.rs`, `scripts/`) is denied at
once, because `coda run` is headless and denies every prompt, so the attempt
fails the verifier. Transcripts are scrubbed by `coda_selfhost::scrub`
(environment values of six or more characters, API keys and absolute paths)
before they are committed. A run against a local model is informational only
(OI-4).
