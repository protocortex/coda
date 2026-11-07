#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Runs a packaged coda binary: it must print its version, and it must open and
# migrate a database written by an old release without losing its rows.
# The fixture is a committed schema version 1 file with one session, because the
# sqlite3 on some runners has no fts5 and could not build it from the schema.
# Usage: scripts/smoke-binary.sh <path-to-coda>
set -euo pipefail

OLD_SCHEMA_VERSION=1
OLD_SESSION_ID=7
FIXTURE_REL="scripts/tests/fixtures/schema-v1.db"
SCHEMA_SOURCE_REL="crates/coda-store/src/lib.rs"
VERSION_PREFIX="coda "

binary="${1:-}"
if [ -z "$binary" ] || [ ! -x "$binary" ]; then
  echo "usage: smoke-binary.sh <path-to-coda>; '$binary' is not an executable file"
  exit 1
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
home="$(mktemp -d)"
trap 'find "$home" -delete' EXIT

version="$("$binary" --version)"
case "$version" in
  "$VERSION_PREFIX"*) echo "$version" ;;
  *)
    echo "--version printed '$version', expected a line starting with '$VERSION_PREFIX'"
    exit 1
    ;;
esac

data="$home/data"
mkdir -m 700 "$data"
db="$data/coda.db"
cp "$root/$FIXTURE_REL" "$db"
chmod 600 "$db"
start="$(sqlite3 "$db" "PRAGMA user_version;")"
if [ "$start" -ne "$OLD_SCHEMA_VERSION" ]; then
  echo "the fixture is at schema version $start, expected $OLD_SCHEMA_VERSION"
  exit 1
fi

# `usage` reads the store without a daemon, and opening it runs the migrations.
CODA_HOME="$home" "$binary" usage >/dev/null

expected="$(sed -n 's/^pub const SCHEMA_VERSION: u32 = \([0-9]*\);$/\1/p' "$root/$SCHEMA_SOURCE_REL")"
if [ -z "$expected" ]; then
  echo "could not read SCHEMA_VERSION from $SCHEMA_SOURCE_REL"
  exit 1
fi
migrated="$(sqlite3 "$db" "PRAGMA user_version;")"
if [ "$migrated" -ne "$expected" ]; then
  echo "the old database is at schema version $migrated after the binary opened it, expected $expected"
  exit 1
fi

kept="$(sqlite3 "$db" "SELECT count(*) FROM sessions WHERE id = $OLD_SESSION_ID;")"
if [ "$kept" -ne 1 ]; then
  echo "the migrated database lost the session that the old one held"
  exit 1
fi

echo "migrated the old database from version $OLD_SCHEMA_VERSION to $migrated"
