#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -u

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/smoke-binary.sh"
WORK_ROOT="$(mktemp -d)"
failures=0

trap 'find "$WORK_ROOT" -delete' EXIT

fail() {
  echo "FAIL: $1"
  failures=$((failures + 1))
}

# A stand-in for coda: `--version` prints a version, and `usage` migrates the
# database the way the real binary does when it opens an old one.
make_stub() {
  local path="$1" version_line="$2" usage_body="$3"
  cat >"$path" <<STUB
#!/usr/bin/env bash
case "\$1" in
  --version) $version_line ;;
  usage) $usage_body ;;
esac
STUB
  chmod +x "$path"
}

GOOD_VERSION='echo "coda 0.1.0"'
LATEST="$(sed -n 's/^pub const SCHEMA_VERSION: u32 = \([0-9]*\);$/\1/p' "$(dirname "$SCRIPT")/../crates/coda-store/src/lib.rs")"
GOOD_USAGE="sqlite3 \"\$CODA_HOME/data/coda.db\" \"PRAGMA user_version = $LATEST\""
HALF_MIGRATED_USAGE="sqlite3 \"\$CODA_HOME/data/coda.db\" \"PRAGMA user_version = 2\""
LOSES_SESSION_USAGE="sqlite3 \"\$CODA_HOME/data/coda.db\" \"DELETE FROM sessions; PRAGMA user_version = $LATEST\""

run_case() {
  local label="$1" want="$2" version_line="$3" usage_body="$4"
  local stub="$WORK_ROOT/coda-$RANDOM"
  make_stub "$stub" "$version_line" "$usage_body"
  # Act
  bash "$SCRIPT" "$stub" >"$WORK_ROOT/out" 2>&1
  local status=$?
  # Assert
  [ "$status" -eq "$want" ] || fail "$label: expected exit $want, got $status"
}

run_case "a binary that starts and migrates the old database" 0 "$GOOD_VERSION" "$GOOD_USAGE"
run_case "a binary that prints no version" 1 "true" "$GOOD_USAGE"
run_case "a binary that fails on the old database" 1 "$GOOD_VERSION" "exit 1"
run_case "a binary that leaves the old database unmigrated" 1 "$GOOD_VERSION" "true"
run_case "a binary that stops migrating early" 1 "$GOOD_VERSION" "$HALF_MIGRATED_USAGE"
run_case "a binary that loses the old session" 1 "$GOOD_VERSION" "$LOSES_SESSION_USAGE"

bash "$SCRIPT" "$WORK_ROOT/missing" >"$WORK_ROOT/out" 2>&1
[ "$?" -eq 1 ] || fail "a missing binary: expected exit 1"

if [ "$failures" -gt 0 ]; then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all smoke-binary tests passed"
