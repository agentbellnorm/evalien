#!/bin/sh
# Sets one key in the evalien-secrets Secret. The value is prompted for, so
# it's never echoed, saved, or put on a command line.
#
#   ./scripts/set-secret.sh GOOGLE_CLIENT_ID
#   ./scripts/set-secret.sh VAULT_KEY --generate
#
# --generate makes a random key, and only if none is set: replacing VAULT_KEY
# makes everything already in the vault unreadable.
#
# The pod reads the Secret when it starts:
#   kubectl -n evalien rollout restart deploy/evalien
set -eu
NAME=${1:?usage: set-secret.sh NAME [--generate]}
NAMESPACE=evalien
SECRET=evalien-secrets

kubectl create namespace "$NAMESPACE" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
EXISTING=$(kubectl -n "$NAMESPACE" get secret "$SECRET" -o "jsonpath={.data.$NAME}" 2>/dev/null || true)

if [ "${2:-}" = "--generate" ]; then
  if [ -n "$EXISTING" ]; then
    echo "$NAME is already set; leaving it alone."
    exit 0
  fi
  VALUE=$(openssl rand -base64 32)
else
  printf "%s: " "$NAME"
  stty -echo; read -r VALUE; stty echo; echo
fi
[ -n "$VALUE" ] || { echo "No value given." >&2; exit 1; }

if kubectl -n "$NAMESPACE" get secret "$SECRET" >/dev/null 2>&1; then
  jq -n --arg key "$NAME" --arg value "$VALUE" '{stringData: {($key): $value}}' \
    | kubectl -n "$NAMESPACE" patch secret "$SECRET" --type merge --patch-file /dev/stdin >/dev/null
else
  jq -n --arg key "$NAME" --arg value "$VALUE" --arg name "$SECRET" --arg ns "$NAMESPACE" \
    '{apiVersion: "v1", kind: "Secret", metadata: {name: $name, namespace: $ns}, stringData: {($key): $value}}' \
    | kubectl apply -f - >/dev/null
fi
unset VALUE
echo "$NAME is set in $SECRET."
