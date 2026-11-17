#!/usr/bin/env bash
# Deploy Taskmaster Commons to the VM. Secrets are read from files, never printed.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/load-env.sh
. "$HERE/lib/load-env.sh"

DRY=0
ENV_FILE=/workspace/taskmaster/secrets.env
COMMONS_ENV=/workspace/taskmaster/commons.env
BINARY=target/release/tm-commons
DUCKDNS=0

usage() {
  cat <<USAGE
Usage: $0 [--dry-run] [--env-file PATH] [--commons-env PATH] [--binary PATH] [--duckdns]
  --dry-run          print the commands without running them
  --env-file PATH    secrets file (default $ENV_FILE)
  --commons-env PATH overrides, loaded after --env-file (default $COMMONS_ENV)
  --binary PATH      tm-commons binary (default $BINARY)
  --duckdns          also update DuckDNS (needs DUCKDNS_TOKEN)
USAGE
}

while [[ $# -gt 0 ]]; do
  case $1 in
    --dry-run) DRY=1 ;;
    --env-file) ENV_FILE=${2:?}; shift ;;
    --commons-env) COMMONS_ENV=${2:?}; shift ;;
    --binary) BINARY=${2:?}; shift ;;
    --duckdns) DUCKDNS=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown flag: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

load_env_file "$ENV_FILE"
[[ -f $COMMONS_ENV ]] && load_env_file "$COMMONS_ENV"

HOST="${HOST:-${VPS_IP:-130.210.90.199}}"
SSH_USER="${SSH_USER:-${VPS_SSH_USER:-opc}}"
SSH_KEY="${SSH_KEY:-${VPS_SSH_KEY:-/workspace/taskmaster/vps_key}}"
DOMAIN="${DOMAIN:-taskmaster-commons.duckdns.org}"

if ! require_vars DATABASE_URL GOOGLE_CLIENT_ID GOOGLE_CLIENT_SECRET GITHUB_CLIENT_ID \
  GITHUB_CLIENT_SECRET TM_COMMONS_SIGNING_KEY TM_COMMONS_SESSION_SECRET; then
  if ((DRY)); then echo "warning: required vars missing (dry-run continues)" >&2
  else echo "error: required vars missing" >&2; exit 1; fi
fi

SSH_OPTS=(-i "$SSH_KEY" -o StrictHostKeyChecking=accept-new)
REMOTE="$SSH_USER@$HOST"

# run CMD...: print in dry-run, execute otherwise. Commands reference paths only.
run() {
  if ((DRY)); then printf '+ %s\n' "$*"; else "$@"; fi
}
rssh() { run ssh "${SSH_OPTS[@]}" "$REMOTE" "$1"; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
CENV="$TMP/commons.env"
CADDY="$TMP/Caddyfile"

# 1) commons.env
write_commons_env "$CENV"
if ((DRY)); then
  echo "# commons.env would contain variables:"
  cut -d= -f1 "$CENV" | sed 's/^/#   /'
fi
sed "s/^commons\.example\.invalid /$DOMAIN /" "$HERE/Caddyfile" >"$CADDY"

# 2) upload
run scp "${SSH_OPTS[@]}" "$BINARY" "$REMOTE:/tmp/tm-commons"
run scp "${SSH_OPTS[@]}" "$CENV" "$REMOTE:/tmp/commons.env"
run scp "${SSH_OPTS[@]}" "$HERE/tm-commons.service" "$REMOTE:/tmp/tm-commons.service"
run scp "${SSH_OPTS[@]}" "$CADDY" "$REMOTE:/tmp/Caddyfile"

# 3) user, binary, env
rssh 'id tm-commons >/dev/null 2>&1 || sudo useradd --system --no-create-home --shell /usr/sbin/nologin tm-commons'
rssh 'sudo install -m 755 /tmp/tm-commons /usr/local/bin/tm-commons && sudo install -d -m 750 -o root -g tm-commons /etc/tm-commons'
rssh 'sudo install -m 600 -o root -g tm-commons /tmp/commons.env /etc/tm-commons/commons.env && rm -f /tmp/commons.env'

# 4) migrations: tm-commons migrates over DATABASE_URL_DIRECT on start (if set);
# the runtime DATABASE_URL stays the Neon pooled string.
rssh "sudo -u tm-commons bash -c 'set -a; . /etc/tm-commons/commons.env; tm-commons check'"

# 5) service
rssh 'sudo install -m 644 /tmp/tm-commons.service /etc/systemd/system/tm-commons.service && sudo systemctl daemon-reload && sudo systemctl enable --now tm-commons'

# 6) caddy
rssh 'sudo install -m 644 /tmp/Caddyfile /etc/caddy/Caddyfile && sudo systemctl reload caddy'

# 7) optional DuckDNS (token is never regenerated)
if ((DUCKDNS)) && [[ -n ${DUCKDNS_TOKEN:-} ]]; then
  sub=${DOMAIN%%.duckdns.org}
  real="curl -fsS \"https://www.duckdns.org/update?domains=$sub&token=$DUCKDNS_TOKEN&ip=\""
  shown="curl -fsS \"https://www.duckdns.org/update?domains=$sub&token=****&ip=\""
  if ((DRY)); then printf '+ ssh %s %s\n' "$REMOTE" "$shown"
  else ssh "${SSH_OPTS[@]}" "$REMOTE" "$real"; echo; fi
elif ((DUCKDNS)); then
  echo "warning: --duckdns given but DUCKDNS_TOKEN unset; skipping" >&2
fi

echo "Summary: HOST=$HOST SSH_USER=$SSH_USER DOMAIN=$DOMAIN$( ((DRY)) && echo ' (dry-run)')"
