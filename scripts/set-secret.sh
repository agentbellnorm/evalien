#!/bin/sh
# Sets one key of the evalien-secrets Secret, by setting the field on the 1Password
# item it's synced from (vault Bushnet, item evalien; see deploy/k8s/08-external-secret.yaml).
# The value is prompted for (or read from a pipe), so it's never echoed, saved, or put
# on a command line.
#
#   ./scripts/set-secret.sh GOOGLE_CLIENT_ID
#   ./scripts/set-secret.sh VAULT_KEY --generate
#
# --generate makes a random key, and only if none is set: replacing VAULT_KEY
# makes everything already in the vault unreadable.
#
# A key that isn't in deploy/k8s/08-external-secret.yaml yet needs an entry there too.
# The cluster syncs within the hour (this asks for it now), and Reloader restarts the pod.
set -eu
NAME=${1:?usage: set-secret.sh NAME [--generate]}
VAULT=${OP_VAULT:-Bushnet}
ITEM=${OP_ITEM:-evalien}

if ITEM_JSON=$(op item get "$ITEM" --vault "$VAULT" --format json 2>/dev/null); then
  EXISTS=1
else
  EXISTS=0
  ITEM_JSON=$(jq -n --arg title "$ITEM" '{title: $title, category: "SECURE_NOTE", fields: []}')
fi
EXISTING=$(printf '%s' "$ITEM_JSON" | jq -r --arg name "$NAME" '[.fields[]? | select(.label == $name) | .value // empty][0] // empty')

if [ "${2:-}" = "--generate" ]; then
  if [ -n "$EXISTING" ]; then
    echo "$NAME is already set; leaving it alone."
    exit 0
  fi
  VALUE=$(openssl rand -base64 32)
elif [ -t 0 ]; then
  printf "%s: " "$NAME"
  stty -echo; read -r VALUE; stty echo; echo
else
  read -r VALUE || true
fi
[ -n "$VALUE" ] || { echo "No value given." >&2; exit 1; }

# The value reaches jq through the environment, not its arguments.
UPDATED=$(printf '%s' "$ITEM_JSON" | SECRET_VALUE="$VALUE" jq --arg name "$NAME" '
  if any(.fields[]?; .label == $name)
  then .fields |= map(if .label == $name then .value = env.SECRET_VALUE else . end)
  else .fields = ((.fields // []) + [{label: $name, type: "CONCEALED", value: env.SECRET_VALUE}])
  end')
unset VALUE EXISTING ITEM_JSON
if [ "$EXISTS" = 1 ]; then
  printf '%s' "$UPDATED" | op item edit "$ITEM" --vault "$VAULT" >/dev/null
else
  printf '%s' "$UPDATED" | op item create --vault "$VAULT" - >/dev/null
fi
unset UPDATED
echo "$NAME is set in 1Password ($VAULT/$ITEM)."

if kubectl -n evalien annotate externalsecret evalien-secrets force-sync="$(date +%s)" --overwrite >/dev/null 2>&1; then
  echo "Sync requested; Reloader restarts the pod once it lands."
else
  echo "Couldn't reach the cluster; it syncs within the hour."
fi
