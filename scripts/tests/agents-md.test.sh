#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# AGENTS.md names scripts and docs. Fails when one of them no longer exists, so the rules cannot point at a file that moved.
set -u

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
RULES="${AGENTS_FILE:-$ROOT/AGENTS.md}"
PATH_PATTERN='(scripts|docs)/[A-Za-z0-9_./-]+\.(sh|md)'
failures=0

if [ ! -f "$RULES" ]; then
  echo "FAIL: $RULES does not exist"
  exit 1
fi

while IFS= read -r path; do
  if [ ! -e "$ROOT/$path" ]; then
    echo "FAIL: AGENTS.md names $path, which does not exist"
    failures=$((failures + 1))
  fi
done < <(grep -oE "$PATH_PATTERN" "$RULES" | sort -u)

for command in "cargo fmt --check" "cargo clippy --workspace --all-targets -- -D warnings" "cargo nextest run --workspace"; do
  if ! grep -qF "$command" "$ROOT/.github/workflows/ci.yml"; then
    echo "FAIL: CI no longer runs '$command'"
    failures=$((failures + 1))
  fi
  if ! grep -qF "$command" "$RULES"; then
    echo "FAIL: AGENTS.md does not list '$command'"
    failures=$((failures + 1))
  fi
done

if [ "$failures" -gt 0 ]; then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all agents-md tests passed"
