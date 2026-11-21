#!/bin/sh
# Sets the two keys evalien always needs, in 1Password (via set-secret.sh):
# - MATRIX_ACCESS_TOKEN: logs in as the agent's Matrix account to get one.
# - ANTHROPIC_API_KEY: from ./.env, or prompted for.
# Nothing is echoed, written to disk, or put on a command line.
set -eu
HOMESERVER=${HOMESERVER:-https://matrix.skum.dev}
MATRIX_USER=${MATRIX_USER:-evalien}
HERE=$(dirname "$0")

printf "Password for %s on %s: " "$MATRIX_USER" "$HOMESERVER"
stty -echo; read -r PASSWORD; stty echo; echo

TOKEN=$(PASSWORD="$PASSWORD" jq -n --arg user "$MATRIX_USER" \
    '{type: "m.login.password", identifier: {type: "m.id.user", user: $user}, password: env.PASSWORD, initial_device_display_name: "evalien pod"}' \
  | curl -fsS "$HOMESERVER/_matrix/client/v3/login" -d @- \
  | jq -r '.access_token // empty')
unset PASSWORD
[ -n "$TOKEN" ] || { echo "Login failed." >&2; exit 1; }
printf '%s\n' "$TOKEN" | "$HERE/set-secret.sh" MATRIX_ACCESS_TOKEN
unset TOKEN

API_KEY=$(grep -s '^ANTHROPIC_API_KEY=' .env | cut -d= -f2- || true)
if [ -n "$API_KEY" ]; then
  printf '%s\n' "$API_KEY" | "$HERE/set-secret.sh" ANTHROPIC_API_KEY
else
  "$HERE/set-secret.sh" ANTHROPIC_API_KEY
fi
unset API_KEY
