#!/usr/bin/env bash
# Sourced helpers for loading secrets from a file without echoing them.

load_env_file() {
  local path=$1
  if [[ ! -f $path ]]; then
    echo "error: env file not found: $path" >&2
    return 1
  fi
  local was_x=
  [[ $- == *x* ]] && was_x=1
  set +x
  set -a
  # shellcheck disable=SC1090
  . "$path"
  set +a
  [[ -n $was_x ]] && set -x
  return 0
}

# Prints **** for a non-empty value, (unset) otherwise.
mask() {
  if [[ -n ${1:-} ]]; then echo '****'; else echo '(unset)'; fi
}

# require_vars NAME... : fails if any is empty or still a replace-me placeholder.
require_vars() {
  local name missing=0
  for name in "$@"; do
    if [[ -z ${!name:-} || ${!name} == *replace-me* ]]; then
      echo "missing: $name" >&2
      missing=1
    fi
  done
  return $missing
}

# env_line NAME VALUE : NAME="VALUE", with \ " $ and ` escaped. Both bash (`. file`) and
# systemd's EnvironmentFile read it back unchanged, so a DATABASE_URL with & or ? in it
# survives. A value with a newline is refused.
env_line() {
  local name=$1 value=$2
  if [[ $value == *$'\n'* ]]; then
    echo "error: $name has a line break" >&2
    return 1
  fi
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  value=${value//\$/\\\$}
  value=${value//\`/\\\`}
  printf '%s="%s"\n' "$name" "$value"
}

# write_commons_env OUTFILE : writes the Commons runtime env. Mode 600 here; the server
# copy is 640 root:tm-commons, so the service (and `tm-commons check` as that user) reads it.
write_commons_env() {
  local out=$1 pair
  local -a pairs=(
    "TM_COMMONS_URL=${TM_COMMONS_URL:-https://${DOMAIN:-}}"
    "DATABASE_URL=${DATABASE_URL:-}"
  )
  [[ -n ${DATABASE_URL_DIRECT:-} ]] && pairs+=("DATABASE_URL_DIRECT=$DATABASE_URL_DIRECT")
  pairs+=(
    "TM_COMMONS_SIGNING_KEY=${TM_COMMONS_SIGNING_KEY:-}"
    "TM_COMMONS_SESSION_SECRET=${TM_COMMONS_SESSION_SECRET:-}"
    "GOOGLE_CLIENT_ID=${GOOGLE_CLIENT_ID:-}"
    "GOOGLE_CLIENT_SECRET=${GOOGLE_CLIENT_SECRET:-}"
    "GITHUB_CLIENT_ID=${GITHUB_CLIENT_ID:-}"
    "GITHUB_CLIENT_SECRET=${GITHUB_CLIENT_SECRET:-}"
    "TM_COMMONS_MODERATORS=${TM_COMMONS_MODERATORS:-${MODERATORS:-}}"
    "TM_COMMONS_BIND=127.0.0.1:8740"
  )
  (
    umask 077
    : >"$out"
    for pair in "${pairs[@]}"; do
      env_line "${pair%%=*}" "${pair#*=}" >>"$out" || exit 1
    done
  ) || { rm -f "$out"; return 1; }
  chmod 600 "$out"
}
