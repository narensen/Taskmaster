#!/bin/sh
# The version lives in one place, `[workspace.package] version` in Cargo.toml. Every other
# copy (the desktop crate, tauri.conf.json, the web app's package.json and lock) must say
# the same thing.
#
#   scripts/version.sh                 print the version
#   scripts/version.sh check [X.Y.Z]   fail when a copy disagrees (or differs from X.Y.Z)
#   scripts/version.sh set X.Y.Z       write X.Y.Z everywhere, then refresh Cargo.lock
set -eu
cd "$(dirname "$0")/.."

source_version() {
  # The first `version = "..."` under [workspace.package].
  awk '/^\[workspace.package\]/{p=1; next} /^\[/{p=0} p && /^version *=/{gsub(/.*= *"|".*/, ""); print; exit}' Cargo.toml
}

copies() {
  printf 'desktop/src-tauri/Cargo.toml %s\n' "$(awk '/^\[package\]/{p=1; next} /^\[/{p=0} p && /^version *=/{gsub(/.*= *"|".*/, ""); print; exit}' desktop/src-tauri/Cargo.toml)"
  for file in desktop/src-tauri/tauri.conf.json web/package.json; do
    printf '%s %s\n' "$file" "$(node -p "require('./$file').version")"
  done
  printf 'web/package-lock.json %s\n' "$(node -p "const l=require('./web/package-lock.json'); l.version + (l.packages[''].version === l.version ? '' : ' (packages: ' + l.packages[''].version + ')')")"
  # Cargo.lock: every workspace crate.
  for crate in tm-core tm-daemon tm-harness tm-review tm-commons tm-dictionary tm-verify tm-token tm-launcher; do
    printf 'Cargo.lock:%s %s\n' "$crate" "$(awk -v name="$crate" '$0 == "name = \"" name "\"" {getline; gsub(/.*= *"|".*/, ""); print; exit}' Cargo.lock)"
  done
}

case "${1:-}" in
  "")
    source_version
    ;;
  check)
    want=$(source_version)
    status=0
    if [ -n "${2:-}" ] && [ "$2" != "$want" ]; then
      echo "Cargo.toml says $want, but the release is $2: run scripts/version.sh set $2" >&2
      status=1
    fi
    copies | while read -r file have; do
      if [ "$have" != "$want" ]; then
        echo "$file says $have, Cargo.toml says $want: run scripts/version.sh set $want" >&2
        echo fail
      fi
    done | grep -q fail && status=1
    [ "$status" = 0 ] && echo "every copy says $want"
    exit "$status"
    ;;
  set)
    new=${2:?usage: scripts/version.sh set X.Y.Z}
    echo "$new" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || { echo "a version looks like 1.2.3, not $new" >&2; exit 1; }
    sed -i.bak "/^\[workspace.package\]/,/^\[/ s/^version *= *\".*\"/version = \"$new\"/" Cargo.toml && rm Cargo.toml.bak
    sed -i.bak "/^\[package\]/,/^\[/ s/^version *= *\".*\"/version = \"$new\"/" desktop/src-tauri/Cargo.toml && rm desktop/src-tauri/Cargo.toml.bak
    # Only the version lines change, so each file keeps its own formatting: the top-level
    # "version" of each, and the lock's copy for the root package.
    VERSION="$new" node -e '
      const fs = require("fs");
      for (const [file, count] of [["desktop/src-tauri/tauri.conf.json", 1], ["web/package.json", 1], ["web/package-lock.json", 2]]) {
        let text = fs.readFileSync(file, "utf8");
        let left = count;
        text = text.replace(/"version": "[^"]*"/g, (hit) => (left-- > 0 ? `"version": "${process.env.VERSION}"` : hit));
        fs.writeFileSync(file, text);
      }'
    # The lockfiles name each workspace crate with its version.
    cargo metadata --format-version 1 >/dev/null
    if [ -f desktop/src-tauri/Cargo.lock ]; then
      sed -i.bak "/^name = \"taskmaster-desktop\"$/{n; s/^version = \".*\"/version = \"$new\"/}" desktop/src-tauri/Cargo.lock && rm desktop/src-tauri/Cargo.lock.bak
    fi
    "$0" check "$new"
    ;;
  *)
    echo "usage: scripts/version.sh [check [X.Y.Z] | set X.Y.Z]" >&2
    exit 2
    ;;
esac
