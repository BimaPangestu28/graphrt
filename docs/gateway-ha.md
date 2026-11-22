# Gateway active/passive (on-prem HA)

Two `graphrt gateway` processes share one SQLite store and one lease on a
volume. One is **active** (holds the lease, opens the store, listens); the
other is **passive** (polls the lease, opens nothing, does not listen). When
the active dies, freezes, loses the volume or shuts down, the passive takes
over; the old active can never write again.

> Status: preview (phase F3, store tier T1-AP). Build with `--features gateway`.
> Replication is left to the block layer (one host, or DRBD / a SAN LUN); the
> gateway never replicates on its own.

## What you get

| Event | What happens | Measured (macOS arm64, see below) |
| --- | --- | --- |
| `kill -9` / crash of the active | the kernel drops its `active.lock`; the passive sees it at its next poll and takes over | 73 ms to the first `200 /healthz` with the default `poll_ms: 200`; 41–53 ms with `poll_ms: 50` |
| A run executing at that moment | its agent notices the lost stream, waits its reattach window, preempts and holds the orphan; the new active polls it, re-dispatches, the run finishes | finished 3.1 s after the kill (an 800 ms tool call, re-executed once) |
| Active frozen (`SIGSTOP`, VM pause, I/O stall) | after `stale_after_ms` without a heartbeat the passive steals the lease, `SIGKILL`s the frozen holder when it runs on the same host. Also when it froze *inside* a heartbeat, with `lease.lock` taken: after `stale_after_ms` of a busy lock the passive decides without it | `stale_after_ms` + 25–50 ms (1.53 s with 1500 ms); frozen inside the lock 1.56–1.89 s (13 runs at load average 59–126) |
| Planned restart / upgrade (`SIGTERM`) | the active stops listening, releases the lease; the passive takes over at its next poll | 26–78 ms |
| Active loses the volume | it cannot confirm its heartbeat, stops writing after `stale_after_ms − heartbeat_ms`, exits after `stale_after_ms`; the passive steals | ≈ `stale_after_ms` |
| Old active comes back | its first write or heartbeat sees a newer token, it exits; the supervisor restarts it as a passive | – |

A passive holds no store and no listener: 11.7 MiB RSS against 14.6 MiB for
an idle active (`min-size` build, macOS arm64; Linux musl not measured yet).
The feature adds 97.6 KiB to the `--features gateway` binary and no
dependency.

Data: nothing is lost on a crash of either process (the WAL is on the shared
volume, SQLite replays it when the new active opens the store). On loss of the
volume itself the newest `backup:` copy is restored (RPO = backup interval).

## How it works

Files in `ha.lease_dir` (default: the store's directory):

| File | Role |
| --- | --- |
| `graphrt-gateway.lease` | JSON: token, holder (node, host, pid), heartbeat counter, `released`. Replaced atomically |
| `graphrt-gateway.lease.lock` | `flock` around each read-modify-write of the lease (write + fsync + rename: 4 ms p50, 8 ms max on macOS APFS, where fsync is `F_FULLFSYNC`) |
| `graphrt-gateway.active.lock` | `flock` held by the active for its whole tenure |
| `graphrt-gateway.promote` | a manual promotion request |

- **Liveness** is the lease. A passive takes it when there is none, when it
  was released, when the holder's `active.lock` is free (the holder process is
  gone), on `graphrt gateway ha promote`, or with `takeover: auto` when the
  heartbeat counter did not move for `stale_after_ms` *measured on the
  passive's own monotonic clock* (wall clocks are never compared). A
  `lease.lock` still taken after `stale_after_ms` means its holder froze
  inside a read-modify-write and keeps the lock until it dies: the passive
  then applies the same rules without the lock (the lease file is replaced
  atomically, so the read is whole), which is what lets it `SIGKILL` that
  holder. Up to 0.4.0 such a freeze — about 2 % of the time with a 200 ms
  heartbeat, more under CPU load — blocked every takeover until the process
  resumed.
- **Safety** is the store. Every take increments the token; the new active
  raises `meta.fence` in SQLite to its token (compare-and-set: only a higher
  token wins), and every write transaction of the gateway first checks that
  `meta.fence` still holds its own token. SQLite serialises write
  transactions, so after a promotion committed, no write of an older active
  can commit, whatever the lease file or the clocks say. A refused write trips
  the old active, which exits (invariant I8).
- **Self-fence.** An active whose last confirmed heartbeat is older than
  `stale_after_ms − heartbeat_ms` refuses writes before any passive may
  consider it stale; a resumed process stops writing before its first beat.
- **Takeover = restart.** The new active does what a restarted single gateway
  does: SQLite replays the WAL, `PRAGMA quick_check` verifies the store (a
  failed check or a missing file restores the newest backup and keeps the
  failed file as `gateway.db.corrupt-<ms>`), queued runs are re-dispatched,
  running runs poll their agents for orphans (the local-process
  provider adopts the agents through their pid files in `socket_dir`), cron
  continues from its stored last fire (a repeated fire has the same run id and
  is a duplicate; `catchup:` decides about fires missed during the takeover),
  and webhook redeliveries meet their marker (`{"duplicate": true}`).

## Configuration

```yaml
listen: 127.0.0.1:8700
store: /srv/graphrt/gateway.db          # on the shared volume
socket_dir: /srv/graphrt/sockets        # shared too, so the new active adopts the agents (keep it short: see gateway.md)
backup: { dir: /backup/graphrt, interval_secs: 300, keep: 12 }
ha:
  node_id: gw-a          # default <host>-<pid>; usually set per process with --ha-node
  lease_dir: /srv/graphrt  # default: the store's directory
  takeover: auto         # auto | manual (manual: only exit, release or `ha promote`)
  heartbeat_ms: 1000
  stale_after_ms: 5000   # >= 3 x heartbeat_ms
  poll_ms: 200           # passive poll period
  kill_stale_holder: true  # SIGKILL a frozen holder on the same host before stealing
  verify_store: true     # quick_check + restore from backup on takeover
  shutdown_grace_ms: 2000  # wait for open connections on SIGTERM before releasing
deployments: [...]
```

`graphrt gateway check gateway.yaml` validates the section. Both processes use
the same file; give each its id on the command line:

```sh
graphrt gateway --config /etc/graphrt/gateway.yaml --ha-node gw-a
graphrt gateway --config /etc/graphrt/gateway.yaml --ha-node gw-b
```

`takeover: auto` is the default because the shared volume is the witness: a
passive can only steal what it can see, and the store fence makes a wrong
guess harmless (one extra failover, never two writers). Choose `manual` when an
operator must decide about a frozen active; a holder that exited is still
replaced at once.

## Supported layouts

**One host, two processes** (protects against crashes, hangs and upgrades of
the gateway, not against losing the host). Same `listen` address for both: the
passive does not bind it until it is active.

**Two hosts, one single-mounter block device**: DRBD (protocol C, synchronous)
or a SAN LUN, managed by Pacemaker (or a similar cluster manager) with node
fencing (STONITH). The cluster manager promotes the device, mounts the file
system, moves the service address and starts the gateway on the surviving
node; the lease and the store fence then work exactly as on one host. Only one
host ever has the volume mounted; agents of the local-process provider die with
their host, so their running runs end `failed{owner_lost}` after
`reclaim_grace_secs` and the client can retry them.

Running a passive on the second host *while* the first host mounts the volume
needs a cluster file system with coherent POSIX locks *and* coherent shared
memory for SQLite's WAL index. No common cluster file system guarantees the
latter, so this layout is **not supported**.

## Do not use

- **NFS (any version), SMB/CIFS, sshfs, s3fs/goofys, GlusterFS, CephFS, EFS,
  Azure Files, Filestore, Kubernetes `ReadWriteMany` volumes.** SQLite in WAL
  mode needs shared memory on one host (`-shm`), and `flock` over network file
  systems is emulated or advisory per client: two hosts can both believe they
  hold the lease and corrupt the database. (Even a single gateway on NFS is
  unsafe in WAL mode.)
- Two hosts mounting the same LUN with a non-cluster file system (ext4/XFS
  mounted twice): silent corruption.
- `kill_stale_holder: false` with `takeover: auto` on one host is safe but
  slower to clean up: the frozen process exits only when it resumes.
- Clock-based fencing (NTP) as a substitute for node fencing across hosts.

In Kubernetes a single gateway on a `ReadWriteOnce` volume (restart < 2 s,
reclaim of running runs) is usually enough; active/passive targets on-prem
hosts.

## systemd

One template unit, one instance per node:

```ini
# /etc/systemd/system/graphrt-gateway@.service
[Unit]
Description=graphrt gateway (%i)
After=network-online.target srv-graphrt.mount
Requires=srv-graphrt.mount

[Service]
User=graphrt
EnvironmentFile=/etc/graphrt/gateway.env
ExecStart=/usr/local/bin/graphrt gateway --config /etc/graphrt/gateway.yaml --ha-node %i
# A superseded active exits; come back as a passive.
Restart=always
RestartSec=1
# SIGTERM: stop listening, release the lease (bounded by ha.shutdown_grace_ms).
KillSignal=SIGTERM
TimeoutStopSec=15
# Agents run in their own process groups and must survive a gateway exit so the
# next active adopts them and reclaims their runs.
KillMode=process
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

```sh
systemctl enable --now graphrt-gateway@gw-a graphrt-gateway@gw-b
systemctl restart graphrt-gateway@gw-a   # planned switchover: gw-b takes over in ~50 ms
```

`/etc/graphrt/gateway.env` holds the variables named by `*_env` in the config
(mode 0600). With Pacemaker, use one instance per host and let the cluster
start it after the file system (`ocf:heartbeat:Filesystem` → `systemd:graphrt-gateway@gw-a`).

## Operations

```sh
graphrt gateway ha status --config /etc/graphrt/gateway.yaml    # lease JSON, tenure lock
graphrt gateway ha promote --config /etc/graphrt/gateway.yaml --node gw-b
```

`ha promote` writes a request the named passive (any passive without
`--node`) consumes at its next poll; it kills the current holder first when
that holder provably runs on its host (it held `active.lock` and still does).

Audit lines on stderr (JSON): `ha_promoted` (token, reason `vacant |
released | holder_exited | requested | stale`, previous holder, `ready_ms`,
reclaimed runs), `ha_killed_stale_holder`, `ha_superseded`, `ha_released`.

## Verification

- `crates/graphrt-gateway/tests/failover_simulation.rs`: deterministic chaos
  on a simulated clock with the real lease state machine, store fence, SQLite
  file and cron/webhook run ids; `kill -9` (also between taking the lease and
  fencing the store), freezes with a pending write, freezes inside the lease
  lock (a heartbeat's write landing late on resume, a passive's poll),
  volume partitions, handovers, promotion requests, clock skew and jumps,
  seeds without the self-fence. 10,000 seeds (`GRAPHRT_SIM_SEEDS=10000`,
  83 s): 131,720 promotions, 455,113 committed writes, 15,780 writes of
  superseded actives refused, 0 violations of I8 (token order of committed
  writes), I3 (one run per fire, no missed fire), I4 (one run per webhook
  key, every delivery acknowledged and stored) and of the freeze bound (on
  one host in `auto`, the lease of an active frozen longer than
  2 × `stale_after_ms` has moved before it resumes); 82,903 single-fault
  takeovers in `auto`: p50 100 ms, p99 5.1 s, max 19.8 s (simulated time,
  bound 30 s). With the store fence disabled the checker reports I8
  violations; with the busy-lock rule removed, 147 freeze-bound violations
  in 1,000 seeds. Pull requests run 1,000 seeds.
- `crates/graphrt-cli/tests/gateway_failover.rs`: two real processes with
  real agents — `kill -9` while a run executes (reclaimed and finished),
  `SIGSTOP` of the active with the lease lock taken (a freeze inside a
  heartbeat, made deterministic) and `SIGTERM`, webhook dedupe across
  failovers, passive RSS.

Limits: a holder frozen inside the lease lock on *another* host (no kill
possible) may finish its heartbeat write when it resumes and overwrite the
new lease; the new active then sees a foreign token and restarts, the old
one is refused by the store fence at its first write and exits, and a
passive takes the lease again — one extra failover, never two actives
committing. External side effects of a
node (LLM calls, `http-request`) stay at-least-once across a failover, as
across any re-dispatch.
