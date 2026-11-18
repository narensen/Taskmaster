#!/usr/bin/env bash
# Build the public site and copy it to the VM's /srv/taskmaster-site. Caddy serves it once
# deploy/commons/deploy.sh has run with SITE_DOMAIN set (or the block in
# deploy/site/Caddyfile is added to /etc/caddy/Caddyfile by hand).
#
#   deploy/site/deploy.sh [--dry-run]
#
# Reads HOST, SSH_USER and SSH_KEY like deploy/commons/deploy.sh (from the same env file).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
. "$ROOT/deploy/commons/lib/load-env.sh"
DRY=0
ENV_FILE=${ENV_FILE:-/workspace/taskmaster/secrets.env}
[[ ${1:-} == --dry-run ]] && DRY=1
[[ -f $ENV_FILE ]] && load_env_file "$ENV_FILE"
HOST="${HOST:-${VPS_IP:-130.210.90.199}}"
SSH_USER="${SSH_USER:-${VPS_SSH_USER:-opc}}"
SSH_KEY="${SSH_KEY:-${VPS_SSH_KEY:-/workspace/taskmaster/vps_key}}"
run() { if ((DRY)); then printf '+ %s\n' "$*"; else "$@"; fi; }

(cd "$ROOT/site" && node build.mjs)
run ssh -i "$SSH_KEY" "$SSH_USER@$HOST" 'sudo install -d -m 755 -o '"$SSH_USER"' /srv/taskmaster-site'
run rsync -az --delete -e "ssh -i $SSH_KEY" "$ROOT/site/dist/" "$SSH_USER@$HOST:/srv/taskmaster-site/"
echo "site: copied to $HOST:/srv/taskmaster-site$( ((DRY)) && echo ' (dry-run)')"
