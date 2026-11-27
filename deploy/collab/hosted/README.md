# Graphrt hosted team server

`https://collab.graphrt.com` runs `graphrt collab serve` with
[`../collab-hosted.yaml`](../collab-hosted.yaml) on a single node k3s
(ingress-nginx, cert-manager with the ClusterIssuer `letsencrypt-prod`, the
`local-path` storage class), in the namespace `graphrt-collab`.

| Piece | What |
| --- | --- |
| `Deployment/collab` | one replica (`Recreate`: one process owns `data_dir`), nonroot, read only root filesystem, 256 MiB memory limit |
| `PersistentVolumeClaim/collab-data` | `data_dir` (`/data`), 5 GiB |
| `Secret/collab-keys` | the signing and secrets keys (made on the server), the Google and Microsoft client ids and secrets |
| `ConfigMap/collab-config` | `collab.yaml` |
| `Ingress/collab` | TLS from cert-manager, one hour WebSocket timeouts |
| `CronJob/collab-backup` | `data_dir` every day at 02:30 UTC to `/var/backups/graphrt-collab` on the host, kept 14 days |

## Deploy

```sh
docker build --build-arg FEATURES=collab -t graphrt-collab:$(git rev-parse --short HEAD) .
DEPLOY_HOST=root@<server> deploy/collab/hosted/deploy.sh
# or a released image:
DEPLOY_HOST=root@<server> deploy/collab/hosted/deploy.sh ghcr.io/graphrt/graphrt:<version>-collab
```

The first deploy makes `Secret/collab-keys` with new keys and empty provider
values. Later deploys keep it. The server starts with invite codes only and
says which provider is off.

## Turn on Google or Microsoft

Register the apps (redirect URI `https://collab.graphrt.com/v1/auth/callback`,
see [docs/collab.md](../../../docs/collab.md#sign-in)), then:

```sh
DEPLOY_HOST=root@<server> deploy/collab/hosted/set-provider.sh google
DEPLOY_HOST=root@<server> deploy/collab/hosted/set-provider.sh microsoft
```

Each asks for the client id and secret, patches the Secret and restarts the
server. `curl https://collab.graphrt.com/v1/info` then lists the provider.

## Backups and restore

```sh
# Check that the newest backup opens (the live server is not touched):
DEPLOY_HOST=root@<server> deploy/collab/hosted/restore.sh test
# Replace data_dir with a backup (takes a backup of the current data first):
DEPLOY_HOST=root@<server> deploy/collab/hosted/restore.sh apply collab-20261128T023000Z.tar.gz
# A backup now:
ssh root@<server> kubectl -n graphrt-collab create job backup-now --from=cronjob/collab-backup
```

The backups hold workspace data and the encrypted workspace secrets. The key
that decrypts them is in `Secret/collab-keys`, not in the backup: keep a copy
of that Secret somewhere safe, apart from the backups.

## Smoke test

`crates/graphrt-collab/tests/live_smoke.rs` checks a deployed server end to
end: two clients co-edit a project, an outsider is refused, and the free
plan's quotas hold. It makes accounts and workspaces, so take a backup first
and restore it after (`restore.sh apply`). Without the variables it does
nothing (CI).

```sh
invite() { ssh root@<server> kubectl -n graphrt-collab exec deploy/collab -c collab -- \
  graphrt collab invite --config /etc/graphrt/collab.yaml --workspace "$1" --role owner --hours 2 | sed -n 's/^code:[[:space:]]*//p'; }
GRAPHRT_COLLAB_LIVE_URL=https://collab.graphrt.com \
GRAPHRT_COLLAB_LIVE_OWNER="$(invite 'Smoke team')" GRAPHRT_COLLAB_LIVE_OUTSIDER="$(invite 'Outsider team')" \
  cargo test -p graphrt-collab --all-features --test live_smoke -- --nocapture
```
