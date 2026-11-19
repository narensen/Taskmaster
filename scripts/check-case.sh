#!/bin/sh
# Fail when two tracked paths differ only by case. macOS and Windows checkouts are
# case-insensitive, so one of the two silently replaces the other there and the build
# breaks (emptyRuns.ts and EmptyRuns.tsx did this once). Folders count too.
set -eu
cd "$(dirname "$0")/.."
clashes=$(git ls-files \
  | awk -F/ '{ p = ""; for (i = 1; i <= NF; i++) { p = (p == "" ? $i : p "/" $i); print p } }' \
  | sort -u \
  | awk '{ key = tolower($0); if (key in seen) { print seen[key] "  and  " $0 } else { seen[key] = $0 } }')
if [ -n "$clashes" ]; then
  echo "These paths differ only by case, which breaks a macOS or Windows checkout:" >&2
  echo "$clashes" >&2
  exit 1
fi
echo "no two paths differ only by case"
