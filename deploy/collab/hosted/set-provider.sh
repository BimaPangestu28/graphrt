#!/usr/bin/env bash
# Set the client id and secret of the Google sign in on the hosted server and
# restart it. The values are read from the terminal (the secret is not echoed),
# go to the server on stdin, and never appear in a file, a log or a command line.
#
# Usage: DEPLOY_HOST=root@<server-ip> deploy/collab/hosted/set-provider.sh google
set -euo pipefail

: "${DEPLOY_HOST:?Set DEPLOY_HOST, e.g. root@203.0.113.10}"
provider="${1:?google}"
case "$provider" in
  google) prefix=GRAPHRT_COLLAB_GOOGLE ;;
  *) echo "unknown provider $provider (google)" >&2; exit 2 ;;
esac
read -r -p "$provider client id: " client_id
read -r -s -p "$provider client secret: " client_secret
echo
# Google client ids and secrets use these characters only.
for value in "$client_id" "$client_secret"; do
  [[ "$value" =~ ^[A-Za-z0-9._~-]+$ ]] || { echo "a value is empty or has unexpected characters" >&2; exit 2; }
done

remote_script=$(cat <<REMOTE
set -euo pipefail
read -r id
read -r secret
printf '{"stringData":{"${prefix}_CLIENT_ID":"%s","${prefix}_CLIENT_SECRET":"%s"}}' "\$id" "\$secret" \
  | kubectl -n graphrt-collab patch secret collab-keys --type merge --patch-file /dev/stdin >/dev/null
kubectl -n graphrt-collab rollout restart deploy/collab >/dev/null
kubectl -n graphrt-collab rollout status deploy/collab --timeout=180s
kubectl -n graphrt-collab logs deploy/collab --tail=3
REMOTE
)
printf '%s\n%s\n' "$client_id" "$client_secret" | ssh "$DEPLOY_HOST" "bash -c $(printf '%q' "$remote_script")"
