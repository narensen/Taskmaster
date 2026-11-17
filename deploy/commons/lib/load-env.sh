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

# write_commons_env OUTFILE : writes the Commons runtime env (mode 600).
write_commons_env() {
  local out=$1
  (
    umask 077
    {
      printf '%s=%s\n' TM_COMMONS_URL "${TM_COMMONS_URL:-https://${DOMAIN:-}}"
      printf '%s=%s\n' DATABASE_URL "${DATABASE_URL:-}"
      if [[ -n ${DATABASE_URL_DIRECT:-} ]]; then
        printf '%s=%s\n' DATABASE_URL_DIRECT "$DATABASE_URL_DIRECT"
      fi
      printf '%s=%s\n' TM_COMMONS_SIGNING_KEY "${TM_COMMONS_SIGNING_KEY:-}"
      printf '%s=%s\n' TM_COMMONS_SESSION_SECRET "${TM_COMMONS_SESSION_SECRET:-}"
      printf '%s=%s\n' GOOGLE_CLIENT_ID "${GOOGLE_CLIENT_ID:-}"
      printf '%s=%s\n' GOOGLE_CLIENT_SECRET "${GOOGLE_CLIENT_SECRET:-}"
      printf '%s=%s\n' GITHUB_CLIENT_ID "${GITHUB_CLIENT_ID:-}"
      printf '%s=%s\n' GITHUB_CLIENT_SECRET "${GITHUB_CLIENT_SECRET:-}"
      printf '%s=%s\n' TM_COMMONS_MODERATORS "${TM_COMMONS_MODERATORS:-${MODERATORS:-}}"
      printf '%s=%s\n' TM_COMMONS_BIND 127.0.0.1:8740
    } >"$out"
  )
  chmod 600 "$out"
}
