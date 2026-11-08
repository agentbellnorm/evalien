#!/bin/sh
# Creates (or replaces) the evalien-secrets Secret in the cluster:
# - MATRIX_ACCESS_TOKEN: logs in as the agent's Matrix account to get one.
# - ANTHROPIC_API_KEY: from ./.env, or prompted for.
# Nothing is echoed or written to disk.
set -eu
HOMESERVER=${HOMESERVER:-https://matrix.skum.dev}
MATRIX_USER=${MATRIX_USER:-evalien}
NAMESPACE=evalien

printf "Password for %s on %s: " "$MATRIX_USER" "$HOMESERVER"
stty -echo; read -r PASSWORD; stty echo; echo

TOKEN=$(jq -n --arg user "$MATRIX_USER" --arg password "$PASSWORD" \
    '{type: "m.login.password", identifier: {type: "m.id.user", user: $user}, password: $password, initial_device_display_name: "evalien pod"}' \
  | curl -fsS "$HOMESERVER/_matrix/client/v3/login" -d @- \
  | jq -r '.access_token // empty')
unset PASSWORD
[ -n "$TOKEN" ] || { echo "Login failed." >&2; exit 1; }

API_KEY=$(grep -s '^ANTHROPIC_API_KEY=' .env | cut -d= -f2- || true)
if [ -z "$API_KEY" ]; then
  printf "Anthropic API key: "
  stty -echo; read -r API_KEY; stty echo; echo
fi

kubectl create namespace "$NAMESPACE" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl -n "$NAMESPACE" create secret generic evalien-secrets \
  --from-literal=MATRIX_ACCESS_TOKEN="$TOKEN" \
  --from-literal=ANTHROPIC_API_KEY="$API_KEY" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null
unset TOKEN API_KEY
echo "evalien-secrets is set in namespace $NAMESPACE."
