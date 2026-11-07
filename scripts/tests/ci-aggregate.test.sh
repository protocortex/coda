#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -u

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/ci-aggregate.sh"
failures=0

fail() {
  echo "FAIL: $1"
  failures=$((failures + 1))
}

expect_status() {
  local want="$1" label="$2"
  shift 2
  bash "$SCRIPT" "$@" >/dev/null 2>&1
  local status=$?
  [ "$status" -eq "$want" ] || fail "$label: expected exit $want, got $status"
}

expect_status 0 "all jobs succeeded" success success success
expect_status 1 "a failed job" success failure success
expect_status 1 "a cancelled job" success cancelled
expect_status 1 "a skipped job" skipped success
expect_status 1 "no results at all"

output="$(bash "$SCRIPT" success failure 2>&1)"
case "$output" in *failure*) ;; *) fail "the bad result is not named" ;; esac

if [ "$failures" -gt 0 ]; then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all ci-aggregate tests passed"
