#!/usr/bin/env bash
# Restore data_dir of the hosted server from a backup of the collab-backup CronJob.
#
# Usage: DEPLOY_HOST=root@<server-ip> deploy/collab/hosted/restore.sh test|apply [backup file name]
#   test   restore into a scratch volume and run `graphrt collab check` on it
#          (the live server is not touched): proves the backup opens.
#   apply  stop the server, replace data_dir with the backup, start it again.
# Without a file name the newest backup in /var/backups/graphrt-collab is used.
set -euo pipefail

: "${DEPLOY_HOST:?Set DEPLOY_HOST, e.g. root@203.0.113.10}"
mode="${1:?test or apply}"
backup="${2:-}"
[[ "$mode" == test || "$mode" == apply ]] || { echo "mode is test or apply" >&2; exit 2; }
[[ -z "$backup" || "$backup" =~ ^collab-[0-9TZ]+\.tar\.gz$ ]] || { echo "not a backup name: $backup" >&2; exit 2; }

ssh "$DEPLOY_HOST" bash -s -- "$mode" "$backup" <<'REMOTE'
set -euo pipefail
mode="$1"; backup="${2:-}"
ns=graphrt-collab
if [[ -z "$backup" ]]; then
  backup="$(ls -1t /var/backups/graphrt-collab/collab-*.tar.gz | head -1 | xargs -n1 basename)"
fi
[[ -f "/var/backups/graphrt-collab/$backup" ]] || { echo "no backup $backup" >&2; exit 1; }
image="$(kubectl -n $ns get deploy collab -o jsonpath='{.spec.template.spec.containers[0].image}')"
echo "backup: $backup, image: $image"
kubectl -n $ns delete pod collab-restore --ignore-not-found >/dev/null

if [[ "$mode" == test ]]; then
  # The live config, data_dir pointed at the restored copy.
  cat <<POD | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: collab-restore, namespace: $ns }
spec:
  restartPolicy: Never
  automountServiceAccountToken: false
  initContainers:
    - name: extract
      image: busybox:1.37
      command: ["sh", "-c", "tar -xzf /backups/$backup -C /restored && sed 's#^data_dir: .*#data_dir: /restored#; s#^listen: .*#listen: 127.0.0.1:18790#' /etc/graphrt/collab.yaml > /work/collab.yaml && chown -R 65532:65532 /restored"]
      volumeMounts:
        - { name: backups, mountPath: /backups, readOnly: true }
        - { name: restored, mountPath: /restored }
        - { name: work, mountPath: /work }
        - { name: config, mountPath: /etc/graphrt, readOnly: true }
  containers:
    - name: check
      image: $image
      args: ["collab", "check", "--config", "/work/collab.yaml"]
      envFrom: [ { secretRef: { name: collab-keys } } ]
      securityContext: { runAsUser: 65532, runAsGroup: 65532, runAsNonRoot: true, allowPrivilegeEscalation: false }
      volumeMounts:
        - { name: restored, mountPath: /restored }
        - { name: work, mountPath: /work, readOnly: true }
  volumes:
    - { name: backups, hostPath: { path: /var/backups/graphrt-collab, type: Directory } }
    - { name: restored, emptyDir: {} }
    - { name: work, emptyDir: {} }
    - { name: config, configMap: { name: collab-config } }
POD
  for _ in $(seq 1 60); do
    phase="$(kubectl -n $ns get pod collab-restore -o jsonpath='{.status.phase}')"
    [[ "$phase" == Succeeded || "$phase" == Failed ]] && break
    sleep 2
  done
  kubectl -n $ns logs collab-restore -c check
  kubectl -n $ns delete pod collab-restore >/dev/null
  [[ "$phase" == Succeeded ]] || { echo "restore test failed ($phase)" >&2; exit 1; }
  echo "restore test ok"
  exit 0
fi

# A backup of the current data first, so an apply can be undone with another apply.
safety="collab-backup-before-restore-$(date -u +%Y%m%d%H%M%S)"
kubectl -n $ns create job "$safety" --from=cronjob/collab-backup >/dev/null
kubectl -n $ns wait --for=condition=complete "job/$safety" --timeout=300s >/dev/null
kubectl -n $ns delete job "$safety" >/dev/null
kubectl -n $ns scale deploy/collab --replicas=0 >/dev/null
kubectl -n $ns wait --for=delete pod -l app=collab --timeout=120s >/dev/null 2>&1 || true
cat <<POD | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: collab-restore, namespace: $ns }
spec:
  restartPolicy: Never
  automountServiceAccountToken: false
  containers:
    - name: restore
      image: busybox:1.37
      command: ["sh", "-c", "set -e; cd /data; ls -A | xargs -r rm -rf; tar -xzf /backups/$backup -C /data; chown -R 65532:65532 /data; ls -la /data"]
      volumeMounts:
        - { name: data, mountPath: /data }
        - { name: backups, mountPath: /backups, readOnly: true }
  volumes:
    - { name: data, persistentVolumeClaim: { claimName: collab-data } }
    - { name: backups, hostPath: { path: /var/backups/graphrt-collab, type: Directory } }
POD
for _ in $(seq 1 60); do
  phase="$(kubectl -n $ns get pod collab-restore -o jsonpath='{.status.phase}')"
  [[ "$phase" == Succeeded || "$phase" == Failed ]] && break
  sleep 2
done
kubectl -n $ns logs collab-restore
kubectl -n $ns delete pod collab-restore >/dev/null
[[ "$phase" == Succeeded ]] || { echo "restore failed ($phase), the server stays stopped" >&2; exit 1; }
kubectl -n $ns scale deploy/collab --replicas=1 >/dev/null
kubectl -n $ns rollout status deploy/collab --timeout=180s
echo "restored $backup (the data before it is in the newest backup)"
REMOTE
