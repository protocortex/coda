# Continuous integration

`.github/workflows/ci.yml` runs on every pull request and every push to `main`. The jobs are `fmt`, `clippy`, `test`, `coverage`, `deny`, `spdx` and `zizmor`. A final `check` job needs all of them, runs even when one fails, and fails unless each one succeeded, so branch protection can require `check` alone.

The `spdx` job also runs the repository's own script checks. You can run each one locally from the repository root:

| Script | What it catches |
| --- | --- |
| `scripts/check-spdx.sh` | A tracked `.rs`, `.sh`, `.yml` or `.yaml` file without the license header. See [licensing](licensing.md). |
| `scripts/check-conflict-markers.sh` | A tracked text file with a line that starts with `<<<<<<< `, `>>>>>>> ` or `|||||||`, which is what a merge conflict leaves behind. A stray marker has reached `main` before. A bare `=======` line is allowed because markdown uses it. |
| `scripts/check-leaked-daemons.sh` | A test that left a daemon running. |
| `scripts/smoke-binary.sh` | Run by the `release` workflow on each packaged binary, see [releasing](release.md). CI runs only its test. |
| `scripts/ci-aggregate.sh` | Turns the results of all jobs into the single `check` status. |

Each script has a test under `scripts/tests/` that the `spdx` job runs as well.

The rules a change must follow, including the ones these checks enforce, are in [AGENTS.md](../AGENTS.md). `scripts/tests/agents-md.test.sh` keeps that file pointing at scripts and docs that exist.
