#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Fails listing every `coda daemon` of this checkout that is still running. Run it
# after the tests: a test that autostarts a daemon must stop it again.
# A daemon that was just told to stop gets LEAK_GRACE_SECONDS to exit.
# The pattern comes from LEAK_PATTERN, never argv: a pattern on our own command line
# would match this process and its parent shell. Both are also filtered out by pid,
# since Linux pgrep skips only itself.
# Usage: [LEAK_PATTERN=<command-line-pattern>] check-leaked-daemons.sh
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
pattern="${LEAK_PATTERN:-$root/target/debug/coda daemon}"
grace="${LEAK_GRACE_SECONDS:-10}"

survivors=""
for _ in $(seq 0 "$grace"); do
  pids="$(pgrep -f -- "$pattern" | grep -vxF -e "$$" -e "$PPID" | paste -sd, - || true)"
  survivors=""
  [ -z "$pids" ] || survivors="$(ps -o pid=,command= -p "$pids" || true)"
  [ -z "$survivors" ] && exit 0
  sleep 1
done
echo "Daemons left running by the tests (stop them with: pkill -f '$pattern'):"
echo "$survivors"
exit 1
