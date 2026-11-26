# Direct deploys to Fly.io (`graphrt deploy --direct`)

> Status: preview. **Not yet verified against real Fly.io**: see
> [Unverified on real Fly](#unverified-on-real-fly).

A direct deploy puts one agent online at `https://<app>.fly.dev` with no
graphrt gateway: the Fly proxy serves `graphrt serve` straight from a Fly
Machine. It is what the desktop app's **Put it online** runs. No image is
built or pushed: every Machine runs the published graphrt image, and the
agent file (a `.grt` bundle or a workflow `.yaml`/`.json`) is written into
the Machine by Fly from the Machine config.

Use the [gateway](gateway.md) with the [Fly provider](gateway-fly.md)
instead when you need durable runs across restarts, webhooks, cron or many
deployments behind one endpoint.

```sh
export FLY_API_TOKEN="$(fly tokens create org personal)"
export OPENAI_API_KEY=sk-...
graphrt deploy --target fly --direct --deployment support-bot --version v1 \
  --region ams --mode scale-to-zero --secret OPENAI_API_KEY support.grt
# [..] preparing: Preparing Fly app support-bot
# ...
# deployed v1 to https://support-bot.fly.dev (POST https://support-bot.fly.dev/v1/workflows/support/runs)
# API key (shown once, store it now): grk_…

curl -s https://support-bot.fly.dev/v1/workflows/support/runs \
  -H "Authorization: Bearer grk_…" -H 'content-type: application/json' \
  -d '{"inputs":{"question":"Where is my order?"}}'
```

The direct deploy is in the default build (cargo feature `deploy-fly`) and
in both release builds (`graphrt-<target>` and `graphrt-gateway-<target>`).

## Getting the CLI

Prebuilt binaries are attached to each GitHub release
(`https://github.com/Graphrt/graphrt/releases/download/v<version>/<asset>`).
Asset names carry the target but **not** the version:

| Platform | Agent build (has `deploy --direct`) | Gateway build |
| --- | --- | --- |
| macOS arm64 | `graphrt-aarch64-apple-darwin.tar.gz` | `graphrt-gateway-aarch64-apple-darwin.tar.gz` |
| macOS x86_64 | `graphrt-x86_64-apple-darwin.tar.gz` | `graphrt-gateway-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 (static musl) | `graphrt-x86_64-unknown-linux-musl.tar.gz` | `graphrt-gateway-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 (static musl) | `graphrt-aarch64-unknown-linux-musl.tar.gz` | `graphrt-gateway-aarch64-unknown-linux-musl.tar.gz` |
| Windows x86_64 | `graphrt-x86_64-pc-windows-msvc.zip` | – |

Each archive holds one directory named like the archive without extension
(`graphrt-aarch64-apple-darwin/`) with `graphrt` (`graphrt.exe`), `LICENSE`,
`README.md` and `CHANGELOG.md`. The release's `SHA256SUMS` lists every asset
in `sha256sum` format (`<64 hex>  <asset name>`, two spaces). Releases are
not signed. `graphrt version` lists `deploy-fly` among the features when
direct deploys are available (releases after 0.5.0).

## Commands

```text
graphrt deploy --target fly --direct --deployment <name> --version <label>
               [--region <code>] [--mode always-on|scale-to-zero] [--secret NAME ...]
               [--org <slug>] [--app <name>] [--image <ref>] [--rotate-api-key] [--json]
               <agent.grt|agent.yaml|agent.json>
graphrt deploy status         --target fly --direct --deployment <name> [--org <slug>] [--app <name>] [--json]
graphrt deploy list-versions  --target fly --direct --deployment <name> [--org <slug>] [--app <name>] [--json]
graphrt deploy rollback <ver> --target fly --direct --deployment <name> [--org <slug>] [--app <name>] [--json]
graphrt deploy pause          --target fly --direct --deployment <name> [--org <slug>] [--app <name>] [--json]
graphrt deploy resume         --target fly --direct --deployment <name> [--org <slug>] [--app <name>] [--json]
graphrt deploy remove         --target fly --direct --deployment <name> [--org <slug>] [--app <name>] [--json]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--deployment` | required | Deployment name. The Fly app is named after it: lowercased, everything but `a-z0-9` turned into `-`, at most 63 characters (`Support Bot` → `support-bot`). |
| `--version` | required | Version label (1–128 printable characters). Re-deploying the same label updates that version's Machine in place. |
| `--region` | Fly's closest | Region of the version's Machine (`ams`, `iad`, `sin`...). |
| `--mode` | `scale-to-zero` | `scale-to-zero`: the Fly proxy stops the Machine when idle and starts it on the next request (cold start of a few seconds). `always-on`: one Machine keeps running. |
| `--secret NAME` | – | Repeatable. Stores the value of the environment variable `NAME` as the Fly secret `NAME`; `graphrt serve` sees it as `$NAME`. Values are never printed or logged. `GRAPHRT_API_KEY` is reserved. |
| `--org` | `personal` | Fly organization slug, used only when the app has to be created. Management subcommands accept it and ignore it (apps are addressed by name). |
| `--app` | from `--deployment` | Use this Fly app name instead (e.g. when the derived name is taken). Pass it to every later command too. |
| `--image` | `ghcr.io/graphrt/graphrt:<this CLI's version>` | Image running `graphrt serve`. |
| `--rotate-api-key` | off | Replace the deployment's API key; the new one is printed once. |
| `--json` | off | JSON Lines on stdout (below). Without it, progress goes to stderr and a summary to stdout. |

Environment: `FLY_API_TOKEN` (required), `GRAPHRT_FLY_API_URL` (Machines API
base URL; default `https://api.machines.dev`, used by tests and for
`http://_api.internal:4280` inside Fly).

## JSON Lines contract

With `--json`, stdout carries only JSON objects, one per line: zero or more
`step` lines, then exactly **one** final `result` or `error` line (a failed
deploy that already stored a new API key prints one
[`api_key` line](#the-api-key-on-failure) right before its `error`). stderr is
free-form (an `error: …` line on failure). Every line validates against
[`schemas/deploy-direct.schema.json`](schemas/deploy-direct.schema.json)
(JSON Schema 2020-12), which the contract tests enforce
(`crates/graphrt-cli/tests/deploy_direct.rs`).

Exit codes: `0` success, `2` invalid input (`invalid_input`), `1` any other
error. Clap's own argument errors (unknown flag, missing value) exit `2`
without a JSON line.

### Progress (`deploy`, `rollback`, `resume`)

```json
{"type":"step","step":"preparing","status":"running","message":"Preparing Fly app support-bot"}
{"type":"step","step":"preparing","status":"done","message":"created app support-bot; 2 public IP(s) allocated"}
{"type":"step","step":"secrets","status":"running","message":"Writing Fly secrets"}
{"type":"step","step":"secrets","status":"done","message":"set GRAPHRT_API_KEY, OPENAI_API_KEY"}
{"type":"step","step":"starting","status":"running","message":"Starting version v1"}
{"type":"step","step":"starting","status":"done","message":"created Machine 148e… and booted it"}
{"type":"step","step":"checking","status":"running","message":"Waiting for GET /readyz to pass"}
{"type":"step","step":"checking","status":"done","message":"version v1 is live; 0 version(s) demoted, 0 pruned"}
```

| `step` | What happens |
| --- | --- |
| `preparing` | local checks (the agent parses, a `.grt` opens and its hashes match, secrets are set in the environment), the app is created when missing, a shared IPv4 and an IPv6 are allocated when missing |
| `secrets` | the `--secret` values and, on a first deploy or `--rotate-api-key`, the API key are written as Fly app secrets in one call (secret *names* appear in the message, never values) |
| `starting` | the version's Machine is created (or updated) and booted |
| `checking` | Fly's `/readyz` readiness check must pass (2 min), then the version becomes current and the previous one standby |

Each step sends `running`, then `done` or `failed`. Local input errors fail
before the first step (only the `error` line is printed). `rollback` and
`resume` report `starting` and `checking` only.

### Results

| Command | Final line |
| --- | --- |
| deploy | `{"type":"result","action":"deploy","url":"https://support-bot.fly.dev","version":"v1","app":"support-bot","workflow":"support","api_key":"grk_…"}` |
| status | see [status](#status-and-list-versions) |
| list-versions | see [list-versions](#status-and-list-versions) |
| rollback | `{"type":"result","action":"rollback","app":…,"url":…,"version":"v1","workflow":"support"}` |
| pause | `{"type":"result","action":"pause","app":…,"url":…,"status":"paused","changed":true}` |
| resume | `{"type":"result","action":"resume","app":…,"url":…,"status":"online","version":"v1","changed":true}` |
| remove | `{"type":"result","action":"remove","app":"support-bot","removed":true}` |

- `api_key` is the deployment's bearer token. It is a string only on the
  deploy that created it (the app had no `GRAPHRT_API_KEY` secret) or with
  `--rotate-api-key`, and `null` on every other deploy: Fly secrets cannot be
  read back, so **store it when you get it**.
- `workflow` is the name to call: `POST {url}/v1/workflows/{workflow}/runs`.
  It is the agent file's stem (`support.grt` → `support`), characters other
  than letters, digits and `_` turned into `-`.
- `status` is `online` (scale-to-zero: also while stopped, it starts on the
  next request), `paused`, `deploying` (a version is being started; a
  candidate older than 15 minutes counts as abandoned), `failed` (the current
  Machine crashed, its check is `critical`, or an always-on Machine is not
  running) or `offline` (the app exists but nothing is current, e.g. after a
  failed first deploy).
- `changed: false`: pause of a paused deployment, resume of a running one.
- `removed: false`: the app did not exist.

### Errors

```json
{"type":"error","code":"health_check_failed","message":"GET /readyz did not pass within 120 s (check readyz is critical: …)"}
```

| `code` | Meaning |
| --- | --- |
| `invalid_input` | bad flag value, unreadable or invalid agent file, agent > 1 MiB, unset `--secret` variable, `--secret NAME=value` (names only), reserved name |
| `missing_token` | `FLY_API_TOKEN` is unset |
| `unauthorized` | Fly refused the token (401/403), e.g. an app deploy token used to create an app |
| `app_name_taken` | another organization owns the app name: choose another `--deployment` (or `--app`) |
| `not_found` | the app, or the version for `rollback`, does not exist; `pause`/`resume` with nothing deployed |
| `capacity` | Fly has no capacity in the region |
| `rate_limited` | Fly kept answering 429 after retries |
| `start_timeout` | the Machine did not boot within 2 minutes, or failed to boot |
| `health_check_failed` | `/readyz` did not pass within 2 minutes; the new Machine was removed and the previous version **keeps serving** |
| `fly_unavailable` | network error or Fly 5xx after retries |
| `fly_api` | another refusal by the Machines API (4xx), with Fly's message |
| `internal` | no random source or async runtime on this machine |

The API key is **never** in an `error` line.

### The API key on failure

A deploy stores a new API key in the `secrets` step (first deploy, or
`--rotate-api-key`). If a later step fails, the key is already live on Fly
and cannot be read back (and the previous one, on rotation, is gone), so it
is not rolled back: it is handed over in one dedicated line printed
immediately before the `error` line:

```json
{"type":"api_key","api_key":"grk_…"}
{"type":"error","code":"health_check_failed","message":"…"}
```

On success there is no `api_key` line: the key is in the result's
`api_key`. Either way it is printed exactly once; the next deploy reports
`api_key: null`.

### status and list-versions

`graphrt deploy status --json`, final line (every field always present, `null` when unknown):

```json
{
  "type": "result",
  "action": "status",
  "app": "support-bot",
  "url": "https://support-bot.fly.dev",
  "status": "online",
  "current_version": "v2",
  "workflow": "support",
  "mode": "scale-to-zero",
  "last_deployed_at": "2026-11-24T16:00:00.123Z",
  "machines": [
    {"id": "148e…", "name": "graphrt-v2-1a2b3c4d", "state": "stopped", "region": "ams",
     "version": "v2", "role": "current", "checks": [{"name": "readyz", "status": "passing"}]},
    {"id": "9080…", "name": "graphrt-v1-5e6f7a8b", "state": "stopped", "region": "ams",
     "version": "v1", "role": "standby", "checks": []}
  ]
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `status` | `online` \| `paused` \| `deploying` \| `failed` \| `offline` | see above |
| `current_version` | string \| null | label of the version serving (null when `offline`) |
| `last_deployed_at` | RFC 3339 UTC string \| null | when the current version was deployed; a rollback keeps that version's original time |
| `mode` | `always-on` \| `scale-to-zero` \| null | mode of the current version |
| `machines[]` | `{id, state, region, …}` | every Machine of the app (current and standby); `state` is Fly's (`started`, `stopped`, `suspended`, `starting`, `failed`…), plus `name`, `version`, `role` (`current`/`candidate`/`standby`) and `checks[{name, status}]` |

`graphrt deploy list-versions --json`, final line (newest deploy first):

```json
{
  "type": "result",
  "action": "list-versions",
  "app": "support-bot",
  "url": "https://support-bot.fly.dev",
  "current_version": "v2",
  "versions": [
    {"label": "v2", "deployed_at": "2026-11-24T16:00:00.123Z", "current": true, "role": "current",
     "mode": "scale-to-zero", "workflow": "support", "state": "stopped"},
    {"label": "v1", "deployed_at": "2026-11-23T09:12:00.000Z", "current": false, "role": "standby",
     "mode": "scale-to-zero", "workflow": "support", "state": "stopped"}
  ]
}
```

`versions[]` always has `label`, `deployed_at`, `current`, plus `role`,
`mode`, `workflow` and `state` (the version's Machine). Only these versions
can be passed to `rollback`.

## How it works

```text
graphrt deploy --direct
  preparing  GET  /v1/apps/{app}                    404 → POST /v1/apps {app_name, org_slug}
             GET  /v1/apps/{app}/ip_assignments     none → POST … {type: v6}, POST … {type: shared_v4}
  secrets    GET  /v1/apps/{app}/secrets            (names only)
             POST /v1/apps/{app}/secrets            {values: {NAME: value, GRAPHRT_API_KEY: key}} → version
  starting   POST /v1/apps/{app}/machines           role=candidate, services, files, min_secrets_version
             GET  /v1/apps/{app}/machines/{id}/wait?state=started
  checking   GET  /v1/apps/{app}/machines/{id}      until every check is passing
             POST /v1/apps/{app}/machines/{id}/metadata/graphrt_role {value: current}   (no reboot)
             previous current: POST …/stop, then POST …/machines/{id} without services, skip_launch
             standby beyond 5 versions: DELETE …/machines/{id}?force=true
```

Each version is **one Machine** whose role is kept in its metadata
(`graphrt_direct=1`, `graphrt_deployment`, `graphrt_version`, `graphrt_role`,
`graphrt_mode`, `graphrt_deployed_at`, `graphrt_workflow`, `graphrt_paused`):

| Role | Public services | State |
| --- | --- | --- |
| `candidate` | yes | being booted and checked |
| `current` | yes | serving; always-on keeps it running, scale-to-zero lets the Fly proxy stop/start it |
| `standby` | **none** | stopped, kept for `rollback` (only its rootfs is billed) |

Old versions have no services, so the Fly proxy never routes to or
autostarts them. A candidate serves alongside the old version from the
moment its readiness check passes until the old one is demoted (a short
blue/green overlap). A candidate that never gets healthy is destroyed (or,
when an existing Machine of the same label was updated, demoted), so the
previous version keeps serving. The five newest standby versions are kept.

The Machine config of a version:

| Field | Value |
| --- | --- |
| `image` | `ghcr.io/graphrt/graphrt:<version>` (or `--image`) |
| `init.cmd` | `serve /workflows --addr 0.0.0.0:8080 --drain-timeout 25s` |
| `files` | `[{guest_path: /workflows/<workflow>.<ext>, raw_value: <base64 of the agent>, mode: 0444}]` |
| `services` | TCP 8080 ← 80 (`http`, `force_https`) and 443 (`tls`,`http`); `autostart: true`; always-on `autostop: off`, `min_machines_running: 1`; scale-to-zero `autostop: stop`, `min_machines_running: 0`; concurrency 20/25 requests |
| `checks.readyz` | HTTP `GET :8080/readyz`, every 10 s, timeout 5 s, grace 5 s, `kind: readiness` |
| `guest` | shared CPU, 1 vCPU, 256 MB |
| `stop_config` | `SIGTERM`, 30 s (`graphrt serve` drains in 25 s, see [deploy.md](deploy.md#graceful-shutdown)) |
| `restart` | `on-failure`, 3 retries |
| `env` | `GRAPHRT_DEPLOYMENT`, `GRAPHRT_DEPLOYMENT_VERSION` (no secrets: those are Fly secrets) |

Pause stops the current Machine and removes its services (the app answers
nothing and nothing autostarts it); resume restores them for the mode it was
deployed with and waits for `/readyz`. Remove deletes the app
(`DELETE /v1/apps/{app}?force=true`), with its Machines, secrets and IPs.

## Secrets and the API key

- `--secret NAME` values travel **only** in the body of
  `POST /v1/apps/{app}/secrets`: not in the Machine config, URLs, progress
  messages or errors. As defence in depth, the CLI replaces any secret value
  (and the unused API key candidate) appearing in a line with `[redacted]`
  before printing it, including Fly error messages that echo a request.
- The API key is `grk_` + 48 hex digits (24 bytes from the OS RNG), stored as
  the Fly secret `GRAPHRT_API_KEY`. `graphrt serve` then requires
  `Authorization: Bearer <key>` on every `/v1` route; `/healthz` and
  `/readyz` stay open (Fly's checks use them).
- Model provider keys are ordinary secrets: `--secret OPENAI_API_KEY`,
  `--secret ANTHROPIC_API_KEY`… (see [deploy.md](deploy.md#configuration)).
  The workflow's own `env:` names can be passed the same way.
- Machines are created with `min_secrets_version` from the secrets update,
  so a Machine never boots with older secrets than those just written.

## Calling the deployment

The deployment is a plain `graphrt serve` ([serving.md](serving.md)) behind
the API key:

| Request | Use |
| --- | --- |
| `POST /v1/workflows/{workflow}/runs` | start a run (`"stream": true` for SSE, `"async": true` for `202 {run_id}`) |
| `GET /v1/runs?limit=&cursor=&workflow=&status=` | recent runs, newest first: `{runs: [{run_id, workflow, status, created_ms, updated_ms, …}], next_cursor}` (`limit` ≤ 500; pass `next_cursor` back as `cursor` for the next page, `null` at the end) |
| `GET /v1/runs/{id}` | one run record (outputs, answer, error, usage) |
| `GET /v1/runs/{id}/events` | SSE replay and live tail |

For a desktop "cloud runs" list: `run_id`, `workflow`, `status`
(`queued`, `running`, `paused`, `succeeded`, `failed`, `aborted`), `created_ms` as the
start time and, for a finished status, `updated_ms` as the end time (Unix ms).

The run store of a direct deploy is **in memory**: runs are listed until the
Machine stops. With scale-to-zero that is a few minutes after the last
request (and on every deploy, rollback or pause). Keep run history you need
on the client, or use the gateway for a durable store.

## Least-privilege tokens

| Operation | Fly token that works |
| --- | --- |
| first deploy (creates the app and allocates IPs) | an **org-scoped** token: `fly tokens create org <org-slug> --expiry 720h` (it can create apps in that organization and manage their Machines, secrets and IPs) |
| later deploys, `status`, `list-versions`, `rollback`, `pause`, `resume` | the org token, or an app deploy token `fly tokens create deploy -a <app>` (Machines and secrets of that one app) |
| `remove` (deletes the app) | the org token (an app-scoped deploy token is not expected to delete its app) |

Prefer a dedicated organization for deployments made from the desktop app,
give its org token an expiry, and revoke it with `fly tokens revoke` when it
is no longer needed. A personal access token (`fly auth token`) also works but
can touch every organization of the user: avoid it. The token is read from
`FLY_API_TOKEN` (never from a flag, so it does not show in process lists) and
is never printed; `FlyV1 fm2_…` macaroons are sent with the `FlyV1` scheme.

## Limits

- Agent file: at most 1 MiB, shipped inline in the Machine config (base64).
  Fly documents no limit for `files`; the cap keeps the request small.
  Bundles needing process or HTTP plugins require Node.js etc. that the
  distroless image does not have: only WebAssembly plugins work.
- One Machine per version; no horizontal scaling, volumes or custom domains
  yet (add them with `fly` on the same app).
- `<app>.fly.dev` can take a minute to resolve after the IPs of a new app
  are allocated; the deploy reports success once the Machine's own
  `/readyz` check passes.
- Only Fly.io is a direct target so far.

## Unverified on real Fly

Direct deploys are in preview. These need a real org token before they are
marked stable (how the tests run is in [CONTRIBUTING.md](../CONTRIBUTING.md#provider-conformance-tests)):

- `POST /v1/apps/{app}/ip_assignments` with `{type: "v6"}` / `{type: "shared_v4"}` (modelled on flyctl's `flaps` client) and the delay until `<app>.fly.dev` answers;
- the error body and status Fly returns for a globally taken app name (matched on `taken` / `already exists`);
- `config.files` with `raw_value` for a 1 MiB file, and its permissions for the `nonroot` user of the image;
- `checks` with `kind: readiness` being reported in `GET /machines/{id}` as `passing`/`critical`, and its timing after boot;
- `min_secrets_version` on create/update, and secrets injected into Machines created through the API;
- `POST …/metadata/{key}` not rebooting the Machine; update with `skip_launch: true` keeping a stopped Machine stopped;
- scale-to-zero autostop/autostart of a Machine created through the API (no `fly.toml`), and the cold start time;
- `DELETE /v1/apps/{app}?force=true`;
- which operations an app deploy token (`fly tokens create deploy`) is allowed (secrets, IPs, app deletion).
