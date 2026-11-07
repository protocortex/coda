#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -u

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/check-conflict-markers.sh"
WORK_ROOT="$(mktemp -d)"
failures=0

# Built from parts so this file never holds a marker line itself.
OURS="<<<<<<""< HEAD"
THEIRS=">>>>>>""> branch"
BASE="||||||""| base"
SEPARATOR="======""="

trap 'find "$WORK_ROOT" -delete' EXIT

fail() {
  echo "FAIL: $1"
  failures=$((failures + 1))
}

new_repo() {
  local dir
  dir="$(mktemp -d "$WORK_ROOT/repo.XXXXXX")"
  git -C "$dir" init -q
  echo "$dir"
}

run_in() {
  (cd "$1" && bash "$SCRIPT" 2>&1)
}

# Scenario: clean tracked files pass.
repo="$(new_repo)"
printf 'one\ntwo\n' >"$repo/clean.txt"
git -C "$repo" add .
# Act
output="$(run_in "$repo")"
status=$?
# Assert
[ "$status" -eq 0 ] || fail "clean files: expected exit 0, got $status"

# Scenario: each marker is reported with its file.
for marker in "$OURS" "$THEIRS" "$BASE"; do
  repo="$(new_repo)"
  printf 'one\n%s\ntwo\n' "$marker" >"$repo/merged.rs"
  git -C "$repo" add .
  # Act
  output="$(run_in "$repo")"
  status=$?
  # Assert
  [ "$status" -eq 1 ] || fail "marker '$marker': expected exit 1, got $status"
  case "$output" in *merged.rs*) ;; *) fail "marker '$marker': file not listed" ;; esac
done

# Scenario: a bare separator line is common in markdown and does not count.
repo="$(new_repo)"
printf 'Title\n%s\n' "$SEPARATOR" >"$repo/doc.md"
git -C "$repo" add .
# Act
output="$(run_in "$repo")"
status=$?
# Assert
[ "$status" -eq 0 ] || fail "separator only: expected exit 0, got $status"

# Scenario: a marker that is not at the start of a line does not count.
repo="$(new_repo)"
printf 'text %s\n' "$OURS" >"$repo/quoted.txt"
git -C "$repo" add .
# Act
output="$(run_in "$repo")"
status=$?
# Assert
[ "$status" -eq 0 ] || fail "marker after text: expected exit 0, got $status"

# Scenario: untracked files are ignored.
repo="$(new_repo)"
printf 'one\n' >"$repo/clean.txt"
git -C "$repo" add .
printf '%s\n' "$OURS" >"$repo/untracked.txt"
# Act
output="$(run_in "$repo")"
status=$?
# Assert
[ "$status" -eq 0 ] || fail "untracked file: expected exit 0, got $status"

if [ "$failures" -gt 0 ]; then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all check-conflict-markers tests passed"
