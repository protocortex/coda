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

Exit codes: 0 accepted; 1 rejected, including a failing `cargo test` (a retry
may help); 2 usage error; 3 the verifier itself failed (git, cargo or file
errors), which a retry loop should not count as a failed attempt.

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

## Live run (parked)

Not run yet: it waits on OI-1 (whether the account's terms allow this headless
use of the Anthropic API) and OI-2 (allow-path provider fixtures). It would use
up to 3 attempts, model `claude-sonnet-5-5`, turn and token caps, and
transcripts scrubbed by `coda_selfhost::scrub`.
