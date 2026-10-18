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
variable named by `--api-key-env`).

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

Status: not run, and not to be run until OI-1 is verified (whether the
account's terms allow this headless use of the Anthropic API). OI-2
(allow-path provider fixtures) is also open. No transcript exists under
`docs/selfhost/`, and none may be written by hand.

```sh
export ANTHROPIC_API_KEY=...        # or name another variable with --api-key-env
scripts/selfhost.sh                 # attempts only, nothing is written
scripts/selfhost.sh --write-transcript
```

`scripts/selfhost.sh` only builds and calls `coda-selfhost live`. The command:

- refuses to start, before it copies or spawns anything, if the key variable is
  not set. It never reads the key itself: the generated `config.toml` holds the
  variable's name (`api_key_env`), and `coda` reads the value.
- runs up to 3 attempts (`--attempts`, 1 to 3), each in a fresh temporary copy
  with a fresh `CODA_HOME`, and stops at the first one the verifier accepts.
- uses the Anthropic provider with model `claude-sonnet-5-5` (`MODEL` in
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
  written to a temporary name and renamed into place. Each transcript has environment values (including the key), `sk-` style keys and
  absolute paths replaced by `<env:NAME>`, `<key>` and `<path>`. Read it before
  committing it.

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
