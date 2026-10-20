#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Records live transcripts from the installed `claude` and `codex` through coda's
# adapters, scrubs them and writes them under crates/coda-testkit/fixtures/.
# Spends subscription quota (about seven short calls), so it needs
# CODA_LIVE_TESTS=1 and never runs in CI. Set KEEP_RAW=1 to keep the unscrubbed
# recordings (they hold paths and ids; delete them yourself).
# Usage: CODA_LIVE_TESTS=1 scripts/live-record.sh
set -euo pipefail

[ "${CODA_LIVE_TESTS:-}" = 1 ] || {
  echo "live-record: set CODA_LIVE_TESTS=1 to spend quota on real CLI calls" >&2
  exit 2
}
cd "$(dirname "$0")/.."

# Short, because the app-server socket path has a length limit.
raw="$(mktemp -d /tmp/cdlive.XXXXXX)"
[ "${KEEP_RAW:-}" = 1 ] || trap 'rm -rf "$raw"' EXIT

CODA_LIVE_OUT="$raw" cargo nextest run -p coda-providers --test live_record \
  --run-ignored only --no-fail-fast </dev/null
cargo build --quiet -p coda-selfhost
scrubber="${CARGO_TARGET_DIR:-target}/debug/coda-selfhost"

user="${USER:-$(id -un)}"
host="$(hostname -s)"
# Only the names the scrubber must know about reach it: no other environment values.
scrub() {
  env -i HOME="$HOME" USER="$user" LOGNAME="$user" HOSTNAME="$host" \
    "$scrubber" scrub "$raw/$1.raw.jsonl" >"$2"
}

claude=crates/coda-testkit/fixtures/claude
codex=crates/coda-testkit/fixtures/codex
mkdir -p "$claude" "$codex"
scrub claude-live-allow "$claude/live-allow.jsonl"
scrub claude-live-deny "$claude/live-deny.jsonl"
scrub claude-live-text "$claude/live-text.jsonl"
scrub claude-live-readonly "$claude/live-readonly.jsonl"
scrub codex-exec-live "$codex/live-exec.jsonl"
scrub codex-app-live-accept "$codex/live-app-server-accept.jsonl"
scrub codex-app-live-decline "$codex/live-app-server-decline.jsonl"

written=("$claude"/live-*.jsonl "$codex"/live-*.jsonl)

# Scrubbing works on text, so a bad replacement could break a line: every line
# must still parse as JSON.
command -v jq >/dev/null || { echo "live-record: jq is required" >&2; exit 2; }
for file in "${written[@]}"; do
  jq empty "$file" >/dev/null 2>&1 || {
    echo "live-record: $file has a line that is not valid JSON after scrubbing" >&2
    rm -f -- "${written[@]}"
    exit 1
  }
done

# A leftover name, home path, host, address or key means the scrubber missed
# something: remove the new fixtures rather than leave them to be committed.
leaks=0
for needle in "$user" "$HOME" "$host" "/Users/" "/home/"; do
  if grep -qiF -- "$needle" "${written[@]}"; then
    echo "live-record: a fixture still contains '$needle'" >&2
    leaks=1
  fi
done
if grep -qE '[A-Za-z0-9._+-]+@[A-Za-z0-9-]+\.[A-Za-z]{2,}|sk-[A-Za-z0-9_-]{16,}|eyJ[A-Za-z0-9_-]{10,}' "${written[@]}"; then
  echo "live-record: a fixture still contains an address or a token" >&2
  leaks=1
fi
if [ "$leaks" = 1 ]; then
  rm -f -- "${written[@]}"
  exit 1
fi
echo "live-record: wrote ${#written[@]} scrubbed fixtures; read them before committing"
