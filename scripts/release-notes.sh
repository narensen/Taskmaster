#!/bin/sh
# Print the release notes for one version: its section of CHANGELOG.md, without the
# heading. Fails when the changelog has no section for it, so a release cannot go out with
# empty notes.
#
#   scripts/release-notes.sh 0.1.5 > notes.md
set -eu
cd "$(dirname "$0")/.."
version=${1:?usage: scripts/release-notes.sh X.Y.Z}
notes=$(awk -v want="## $version" '
  $0 == want { on = 1; next }
  on && /^## / { exit }
  on { print }
' CHANGELOG.md)
if [ -z "$(printf '%s' "$notes" | tr -d '[:space:]')" ]; then
  echo "CHANGELOG.md has no section for $version" >&2
  exit 1
fi
printf '%s\n' "$notes" | sed -e '/./,$!d'
