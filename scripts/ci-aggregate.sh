#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Takes the result of every CI job as an argument and fails unless all of them succeeded.
# A skipped or cancelled job counts as a failure, so the aggregate check cannot go green by accident.
set -euo pipefail

PASS="success"

if [ "$#" -eq 0 ]; then
  echo "No job results were given, so there is nothing to confirm."
  exit 1
fi

bad=()
for result in "$@"; do
  if [ "$result" != "$PASS" ]; then
    bad+=("$result")
  fi
done

if [ "${#bad[@]}" -gt 0 ]; then
  echo "Jobs that did not succeed: ${bad[*]}"
  exit 1
fi
