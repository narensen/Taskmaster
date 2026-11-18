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
SEARXNG=1

usage() {
  cat <<USAGE
Usage: $0 [--dry-run] [--env-file PATH] [--commons-env PATH] [--binary PATH] [--duckdns] [--no-searxng]
  --dry-run          print the commands without running them
  --env-file PATH    secrets file (default $ENV_FILE)
  --commons-env PATH overrides, loaded after --env-file (default $COMMONS_ENV)
  --binary PATH      tm-commons binary (default $BINARY)
  --duckdns          also update DuckDNS (needs DUCKDNS_TOKEN)
  --no-searxng       skip installing the localhost-only SearXNG
USAGE
}

while [[ $# -gt 0 ]]; do
  case $1 in
    --dry-run) DRY=1 ;;
    --env-file) ENV_FILE=${2:?}; shift ;;
    --commons-env) COMMONS_ENV=${2:?}; shift ;;
    --binary) BINARY=${2:?}; shift ;;
    --duckdns) DUCKDNS=1 ;;
    --no-searxng) SEARXNG=0 ;;
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
write_commons_env "$CENV" || exit 1
if ((DRY)); then
  echo "# commons.env would contain variables:"
  cut -d= -f1 "$CENV" | sed 's/^/#   /'
fi
sed "s/^taskmaster-commons\.duckdns\.org /$DOMAIN /" "$HERE/Caddyfile" >"$CADDY"

# 2) upload
run scp "${SSH_OPTS[@]}" "$BINARY" "$REMOTE:/tmp/tm-commons"
run scp "${SSH_OPTS[@]}" "$CENV" "$REMOTE:/tmp/commons.env"
run scp "${SSH_OPTS[@]}" "$HERE/tm-commons.service" "$REMOTE:/tmp/tm-commons.service"
run scp "${SSH_OPTS[@]}" "$CADDY" "$REMOTE:/tmp/Caddyfile"

# 3) check first, with the uploaded binary and env, before anything on the server changes.
# It reads the settings, reaches Neon and creates the tables (over DATABASE_URL_DIRECT when
# set). A failure stops here and leaves the running service and its files as they were.
if ! rssh "chmod 600 /tmp/commons.env && bash -c 'set -a; . /tmp/commons.env; set +a; chmod 755 /tmp/tm-commons; /tmp/tm-commons check'"; then
  rssh 'rm -f /tmp/commons.env' || true
  echo "error: tm-commons check failed; nothing on $HOST was changed" >&2
  exit 1
fi

# 4) user, binary, env (640 root:tm-commons: the service's group reads it)
rssh 'id tm-commons >/dev/null 2>&1 || sudo useradd --system --no-create-home --shell /usr/sbin/nologin tm-commons'
rssh 'sudo install -m 755 /tmp/tm-commons /usr/local/bin/tm-commons && sudo install -d -m 750 -o root -g tm-commons /etc/tm-commons'
rssh 'sudo install -m 640 -o root -g tm-commons /tmp/commons.env /etc/tm-commons/commons.env && rm -f /tmp/commons.env'

# 5) service: restart, so a unit that is already running picks up the new binary and env.
rssh 'sudo install -m 644 /tmp/tm-commons.service /etc/systemd/system/tm-commons.service && sudo systemctl daemon-reload && sudo systemctl enable tm-commons && sudo systemctl restart tm-commons'
if ! rssh 'for i in 1 2 3 4 5 6 7 8 9 10; do systemctl is-active --quiet tm-commons && exit 0; sleep 1; done; sudo journalctl -u tm-commons -n 30 --no-pager; exit 1'; then
  echo "error: tm-commons did not stay up after the restart (its log is above)" >&2
  exit 1
fi

# 6) caddy
rssh 'sudo install -m 644 /tmp/Caddyfile /etc/caddy/Caddyfile && sudo systemctl reload caddy'

# 6b) SearXNG, localhost only (secret key is generated on the VM, never here)
if ((SEARXNG)); then
  run scp "${SSH_OPTS[@]}" "$HERE/searxng/settings.yml" "$REMOTE:/tmp/settings.yml"
  run scp "${SSH_OPTS[@]}" "$HERE/searxng/searxng.service" "$REMOTE:/tmp/searxng.service"
  rssh 'set -e
ENGINE=$(command -v podman || command -v docker || true)
if [ -z "$ENGINE" ]; then sudo dnf install -y podman || sudo apt-get install -y docker.io; ENGINE=$(command -v podman || command -v docker); fi
ENGINE=$(basename "$ENGINE")
sudo install -d -m 755 /etc/searxng
sudo install -m 644 /tmp/settings.yml /etc/searxng/settings.yml
sudo sed -i "s/__SECRET_KEY__/$(openssl rand -hex 32)/" /etc/searxng/settings.yml
sudo install -m 644 /tmp/searxng.service /etc/systemd/system/searxng.service
sudo install -d -m 755 /etc/systemd/system/searxng.service.d
printf "[Service]\nEnvironment=ENGINE=%s\n" "$ENGINE" | sudo tee /etc/systemd/system/searxng.service.d/engine.conf >/dev/null
sudo systemctl daemon-reload
sudo systemctl enable --now searxng
sleep 5
curl -fsS "http://127.0.0.1:8888/search?q=test&format=json" >/dev/null && echo searxng ok'
fi

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
