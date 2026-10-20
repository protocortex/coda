#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Live self-hosting acceptance: up to 3 attempts of the pinned task through
# `coda run` against the Anthropic API. Spends real tokens; see
# docs/selfhost.md. Pass --write-transcript to commit the
# scrubbed transcript under docs/selfhost/. Usage: selfhost.sh [live options]
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build --quiet -p coda-cli -p coda-selfhost
exec target/debug/coda-selfhost live --coda target/debug/coda --source . "$@"
