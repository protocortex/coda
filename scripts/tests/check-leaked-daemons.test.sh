#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
set -u

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/check-leaked-daemons.sh"
WORK_ROOT="$(mktemp -d)"
failures=0
child=""

cleanup() {
  [ -z "$child" ] || kill "$child" 2>/dev/null
  find "$WORK_ROOT" -delete
}
trap cleanup EXIT

fail() {
  echo "FAIL: $1"
  failures=$((failures + 1))
}

# A stand-in daemon: a script whose command line the check can match.
printf '#!/bin/sh\nsleep "$1"\n' >"$WORK_ROOT/coda"
chmod +x "$WORK_ROOT/coda"
pattern="$WORK_ROOT/coda 600"

# Scenario: no matching process is running.
# Act
output="$(LEAK_PATTERN="$pattern" LEAK_GRACE_SECONDS=1 bash "$SCRIPT" 2>&1)"
status=$?
# Assert
[ "$status" -eq 0 ] || fail "no survivors: expected exit 0, got $status"

# Scenario: a matching process is still running.
"$WORK_ROOT/coda" 600 &
child=$!
# Act
output="$(LEAK_PATTERN="$pattern" LEAK_GRACE_SECONDS=1 bash "$SCRIPT" 2>&1)"
status=$?
# Assert
[ "$status" -eq 1 ] || fail "a survivor: expected exit 1, got $status"
case "$output" in *"$child"*) ;; *) fail "the survivor's pid is not listed" ;; esac

if [ "$failures" -gt 0 ]; then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all check-leaked-daemons tests passed"
