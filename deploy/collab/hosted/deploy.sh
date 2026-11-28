#!/usr/bin/env bash
# Ship the collab image and config to the k3s host and roll the deployment.
#
# Usage: DEPLOY_HOST=root@<server-ip> deploy/collab/hosted/deploy.sh [image]
#   image: a local docker image (default graphrt-collab:<git short sha>, built with
#   `docker build --build-arg FEATURES=collab -t graphrt-collab:<sha> .`), or a
#   registry image such as ghcr.io/graphrt/graphrt:0.6.4-collab (pulled by k3s).
#
# The Secret `collab-keys` is made on the server once (signing and secrets keys
# from openssl, empty provider ids and secrets), never leaves it, and is kept on
# later deploys. Fill the provider values with set-provider.sh.
set -euo pipefail

: "${DEPLOY_HOST:?Set DEPLOY_HOST, e.g. root@203.0.113.10}"
SSH_COMMAND="${SSH_COMMAND:-ssh -o ControlMaster=auto -o ControlPath=$HOME/.ssh/cm-%C -o ControlPersist=300 -o ConnectTimeout=30 -o ServerAliveInterval=10}"
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
image="${1:-graphrt-collab:$(git -C "$root" rev-parse --short HEAD)}"
config_hash="$(sha256sum "$root/deploy/collab/collab-hosted.yaml" | cut -c1-16)"

if [[ "$image" != */* ]]; then
  echo "==> Importing $image into k3s on $DEPLOY_HOST"
  docker save "$image" | $SSH_COMMAND "$DEPLOY_HOST" "k3s ctr images import - >/dev/null"
  image="docker.io/library/$image"
fi

echo "==> Applying (image $image)"
$SSH_COMMAND "$DEPLOY_HOST" "mkdir -p /opt/graphrt-collab"
cat "$root/deploy/collab/collab-hosted.yaml" | $SSH_COMMAND "$DEPLOY_HOST" "cat > /opt/graphrt-collab/collab.yaml"
sed -e "s#IMAGE#$image#" -e "s#CONFIG_HASH#$config_hash#" "$here/k8s.yaml" \
  | $SSH_COMMAND "$DEPLOY_HOST" "cat > /opt/graphrt-collab/k8s.yaml"
$SSH_COMMAND "$DEPLOY_HOST" bash -s <<'REMOTE'
set -euo pipefail
cd /opt/graphrt-collab
kubectl create namespace graphrt-collab --dry-run=client -o yaml | kubectl apply -f - >/dev/null
if ! kubectl -n graphrt-collab get secret collab-keys >/dev/null 2>&1; then
  # The keys go to kubectl on stdin, never on a command line.
  signing="$(openssl rand -hex 32)"
  secrets="$(openssl rand -hex 32)"
  printf '%s\n' "GRAPHRT_COLLAB_SIGNING_KEY=$signing" "GRAPHRT_COLLAB_SECRETS_KEY=$secrets" \
    GRAPHRT_COLLAB_GOOGLE_CLIENT_ID= GRAPHRT_COLLAB_GOOGLE_CLIENT_SECRET= \
    | kubectl -n graphrt-collab create secret generic collab-keys --from-env-file=/dev/stdin >/dev/null
  unset signing secrets
  echo "created the collab-keys secret"
fi
kubectl -n graphrt-collab create configmap collab-config --from-file=collab.yaml --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl apply -f k8s.yaml
kubectl -n graphrt-collab rollout status deploy/collab --timeout=180s
kubectl -n graphrt-collab logs deploy/collab --tail=5
REMOTE
echo "==> Deployed $image"
