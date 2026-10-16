#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
set -u

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/check-spdx.sh"
HEADER="SPDX-License-Identifier: AGPL-3.0-only"
WORK_ROOT="$(mktemp -d)"
failures=0

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

# Scenario: every tracked .rs and .sh file has the header.
repo="$(new_repo)"
echo "// $HEADER" >"$repo/good.rs"
printf '#!/usr/bin/env bash\n# %s\n' "$HEADER" >"$repo/good.sh"
git -C "$repo" add .
# Act
output="$(cd "$repo" && bash "$SCRIPT" 2>&1)"
status=$?
# Assert
[ "$status" -eq 0 ] || fail "good files: expected exit 0, got $status"

# Scenario: bad files are listed and the exit code is 1.
repo="$(new_repo)"
echo "// $HEADER" >"$repo/good.rs"
echo "fn main() {}" >"$repo/bad.rs"
echo "echo hi" >"$repo/bad.sh"
git -C "$repo" add .
# Act
output="$(cd "$repo" && bash "$SCRIPT" 2>&1)"
status=$?
# Assert
[ "$status" -eq 1 ] || fail "bad files: expected exit 1, got $status"
case "$output" in *bad.rs*) ;; *) fail "bad.rs not listed" ;; esac
case "$output" in *bad.sh*) ;; *) fail "bad.sh not listed" ;; esac
case "$output" in *good.rs*) fail "good.rs wrongly listed" ;; esac

# Scenario: a header past line 3 does not count.
repo="$(new_repo)"
printf 'a\nb\nc\n// %s\n' "$HEADER" >"$repo/late.rs"
git -C "$repo" add .
# Act
output="$(cd "$repo" && bash "$SCRIPT" 2>&1)"
status=$?
# Assert
[ "$status" -eq 1 ] || fail "late header: expected exit 1, got $status"

# Scenario: untracked files are ignored.
repo="$(new_repo)"
echo "// $HEADER" >"$repo/good.rs"
git -C "$repo" add .
echo "fn main() {}" >"$repo/untracked.rs"
# Act
output="$(cd "$repo" && bash "$SCRIPT" 2>&1)"
status=$?
# Assert
[ "$status" -eq 0 ] || fail "untracked file: expected exit 0, got $status"

if [ "$failures" -gt 0 ]; then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all check-spdx tests passed"
