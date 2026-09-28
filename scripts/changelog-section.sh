#!/usr/bin/env bash
# Print the entries of one CHANGELOG.md section (`## <version>`), without its
# heading: the release workflow uses this as the GitHub release body.
set -euo pipefail

version="${1:?usage: $0 <version>}"
changelog="${2:-CHANGELOG.md}"

section=$(awk -v v="$version" '
  /^## / { if (found) exit; found = ($2 == v); next }
  found
' "$changelog" | sed -e '/./,$!d')

if [ -z "$section" ]; then
  echo "error: no \"## $version\" section in $changelog" >&2
  exit 1
fi
printf '%s\n' "$section"
