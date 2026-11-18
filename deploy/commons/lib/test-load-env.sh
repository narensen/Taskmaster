#!/usr/bin/env bash
# Checks write_commons_env: values with &, $, quotes and backslashes come back unchanged
# when the file is sourced the way deploy.sh and the service's check source it.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/load-env.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export DATABASE_URL='postgresql://u:p@ep-x-pooler.example/neondb?sslmode=require&channel_binding=require'
export DATABASE_URL_DIRECT='postgres://u:p$w"d`x\y@host/db?a=1&b=2'
export TM_COMMONS_URL=https://commons.example TM_COMMONS_SIGNING_KEY=key TM_COMMONS_SESSION_SECRET='s p a c e'
export GOOGLE_CLIENT_ID=g GOOGLE_CLIENT_SECRET=gs GITHUB_CLIENT_ID=h GITHUB_CLIENT_SECRET=hs TM_COMMONS_MODERATORS=github:someone
write_commons_env "$tmp/commons.env"
[[ $(stat -c %a "$tmp/commons.env") == 600 ]] || { echo "FAIL: mode"; exit 1; }
got=$(env -i bash -c "set -a; . '$tmp/commons.env'; set +a; printf '%s\n%s\n%s' \"\$DATABASE_URL\" \"\$DATABASE_URL_DIRECT\" \"\$TM_COMMONS_SESSION_SECRET\"")
want=$(printf '%s\n%s\n%s' "$DATABASE_URL" "$DATABASE_URL_DIRECT" "$TM_COMMONS_SESSION_SECRET")
[[ $got == "$want" ]] || { echo "FAIL: values changed"; diff <(echo "$want") <(echo "$got"); exit 1; }
if (export TM_COMMONS_MODERATORS=$'a\nb'; write_commons_env "$tmp/bad.env") 2>/dev/null; then echo "FAIL: a line break was written"; exit 1; fi
echo "ok: commons.env round-trips"
