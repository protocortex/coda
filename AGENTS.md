# Working in coda

Rules for anyone changing this repository, human or agent. CI enforces the ones marked (CI).

## Before you push

Run the checks CI runs, from the repository root:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
bash scripts/check-spdx.sh
bash scripts/check-conflict-markers.sh
```

`cargo test --workspace` works too if you do not have nextest. See [docs/ci.md](docs/ci.md) for each job and script.

## Tests

- Write the failing test first, run it and see it fail for the reason you expect, then write the fix.
- Name a test for the behavior, as a sentence: `a_failed_edit_leaves_the_index_alone`.
- Lay a test out as `// Arrange`, `// Act`, `// Assert`. Assert one behavior.
- Never wait on the wall clock to make a test pass. Block on the event (a child exiting, a message arriving) or use a seam such as `Sleeper` or `Clock`. `std::thread::sleep` is banned by `clippy.toml` (CI). For a child process use `coda_testkit::wait::output_within`. For an event with nothing to block on, such as a file appearing, use `coda_testkit::wait::poll_until`.
- A time limit in a test is an upper bound, not a duration. Make it generous, because a loaded machine stalls for seconds.
- Keep real time only for tests that run a real subprocess.
- A flaky test is a bug. Do not retry it or add a sleep. Find the race and remove it.
- Tests never use a real model API or an API key. A live check must be opt-in and use the installed `claude` or `codex` login, as described in [docs/live-recording.md](docs/live-recording.md).
- Leave no daemon running after a test (CI checks with `scripts/check-leaked-daemons.sh`).

## Code

- Name every magic value as a constant. Do not repeat a literal.
- Write no comment unless it explains a why the code cannot show. Do not restate what the code does.
- Suppress a lint with `#[expect(..., reason = "...")]`. Every suppression needs a reason (CI), and an expectation that is no longer needed warns, so remove it. Use `allow` only where an `expect` cannot hold, such as a test helper that each test binary uses differently.
- No `unsafe` (the workspace forbids it).
- Pin every dependency with an exact version (`=1.2.3`) in the `Cargo.toml` that declares it. `cargo deny check` must pass (CI).
- Start every new `.rs`, `.sh`, `.yml` and `.yaml` file with `SPDX-License-Identifier: Apache-2.0` in the first three lines (CI).
- Never leave a merge conflict marker in a file (CI).
- Write shell scripts for `bash` that stop on an unset variable or a failed step, and give each a test in `scripts/tests/`. Do not depend on `jq`.

## Writing

- Use plain, simple English in code comments, docs, commit messages and pull request text. Give the reason for a change, not only what changed.
- Do not use em dashes or en dashes.
- Add a line under `[Unreleased]` in `CHANGELOG.md` for every change a user can see, under `Added`, `Changed` or `Fixed`. Update the docs under `docs/` and the README when you change what they describe.

## Commits and pull requests

- One logical change per pull request, with one commit. Sign it (`git commit -S -s`).
- Use a conventional commit title: `feat(daemon): ...`, `fix(cli): ...`, `test: ...`, `ci: ...`, `docs: ...`. Put `Closes #N` in the pull request body.
- Do not merge with failing checks. If main moved, rebase and run the checks again.
