#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Fails listing every tracked .rs, .sh, .yml and .yaml file without the license header.
set -euo pipefail

HEADER="SPDX-License-Identifier: AGPL-3.0-only"
HEADER_LINES=3

files=$(git ls-files -z '*.rs' '*.sh' '*.yml' '*.yaml' | tr '\0' '\n')

missing=()
while IFS= read -r file; do
  [ -n "$file" ] || continue
  if ! head -n "$HEADER_LINES" "$file" | grep -qF "$HEADER"; then
    missing+=("$file")
  fi
done <<< "$files"

if [ "${#missing[@]}" -gt 0 ]; then
  echo "Missing '$HEADER' in the first $HEADER_LINES lines of:"
  printf '  %s\n' "${missing[@]}"
  exit 1
fi
