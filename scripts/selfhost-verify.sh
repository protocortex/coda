#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Accepts or rejects a finished self-hosting attempt: the diff touches
# crates/coda-cli/src/main.rs and adds a test, `cargo test` passes and
# `coda version --json` matches the crate metadata. The checks live in
# crates/coda-selfhost. Usage: selfhost-verify.sh --workdir DIR [--target-dir DIR] [-- CARGO_TEST_ARGS]
set -euo pipefail

# No cd: a relative --workdir or --target-dir stays relative to the caller.
exec cargo run --quiet --manifest-path "$(dirname "$0")/../Cargo.toml" -p coda-selfhost -- verify "$@"
