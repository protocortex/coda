#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Fails listing every tracked text file with a line that starts with a merge conflict marker.
# A bare separator line is left alone because markdown uses it.
set -uo pipefail

PATTERN='^(<<<<<<< |>>>>>>> |\|\|\|\|\|\|\|)'

hits=$(git grep -nIE "$PATTERN")
status=$?

if [ "$status" -eq 0 ]; then
  echo "Conflict markers found:"
  printf '  %s\n' "$hits"
  exit 1
fi
if [ "$status" -ne 1 ]; then
  echo "git grep failed with status $status"
  exit "$status"
fi
