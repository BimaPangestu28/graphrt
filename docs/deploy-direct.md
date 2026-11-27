# Direct deploys to Fly.io (`graphrt deploy --direct`)

> Tested on real Fly.io: see [Tested on real Fly](#tested-on-real-fly).

A direct deploy puts one agent online at `https://<app>.fly.dev` with no
graphrt gateway: the Fly proxy serves `graphrt serve` straight from a Fly
Machine. It is what the desktop app's **Put it online** runs. No image is
built or pushed: every Machine runs the published graphrt image, and the
agent file (a `.grt` bundle or a workflow `.yaml`/`.json`) is written into
the Machine by Fly from the Machine config. An agent larger than 1 MiB, such as
a bundle with its knowledge, is uploaded onto a Fly volume instead: see
[Large agents](#large-agents-and-their-knowledge).

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
               [--chat [--chat-access key|guests] [--chat-title <text>]
                       [--chat-guest-rate-limit <n>] [--chat-guest-budget <n>]]
               [--cors-origin <origin> ...] [--conversations]
               [--monitor] [--devices] [--approvals] [--expire-approvals] [--push <contact>]
               [--channel telegram|whatsapp|slack|teams ...] [--drop-store]
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
| `--chat`, `--monitor`, `--push`... | off | What the deployment serves besides the API: see [Chat page, monitoring and the store](#chat-page-monitoring-and-the-store). |

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
| deploy | `{"type":"result","action":"deploy","url":"https://support-bot.fly.dev","version":"v1","app":"support-bot","workflow":"support","serve":{…},"store":null,"api_key":"grk_…"}` |
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
- `serve` says what the version serves besides the API, and `store` is the
  deployment's volume or `null`: see
  [Chat page, monitoring and the store](#chat-page-monitoring-and-the-store).
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
| `invalid_input` | bad flag value, unreadable or invalid agent file, an agent > 1 MiB without `--region` or with `--drop-store`, unset `--secret` variable, `--secret NAME=value` (names only), reserved name |
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
  "store": null,
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
| `store` | `{volume, size_gb, region, in_use}` \| null | the [store volume](#the-store-volume), when the deployment has one |
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

## Chat page, monitoring and the store

A direct deployment can serve what `graphrt serve` offers to browsers and
phones. Each flag is the `graphrt serve` flag of the same name, and each is
off unless given, on every deploy: a deploy without `--chat` turns the chat
page off again.

| Flag | What the deployment gets | Docs |
| --- | --- | --- |
| `--chat` | A chat page at `{url}/chat/` and the widget `{url}/chat/embed.js` for the deployed agent | [web.md](web.md#the-chat-page) |
| `--chat-access key\|guests` | Who may chat: holders of the API key (default), or anyone who opens the page | [web.md](web.md#who-may-chat) |
| `--chat-title TEXT` | Title of the page and the widget | |
| `--chat-guest-rate-limit N`, `--chat-guest-budget N` | Runs per minute of one guest (default 10) and of all guests together (default 120) | [web.md](web.md#guests) |
| `--cors-origin ORIGIN` | A site whose pages may call the agent and show the widget. Repeatable | [web.md](web.md#cross-origin-requests-cors) |
| `--conversations` | The server keeps the chat history. Uses the store | [web.md](web.md#conversations-kept-by-the-server) |
| `--monitor` | The monitoring page at `{url}/monitor/`, device pairing and the approvals inbox. Uses the store | [monitoring.md](monitoring.md) |
| `--devices` | Device tokens only (`/v1/devices`), for your own app. Uses the store | [monitoring.md](monitoring.md#devices-a-token-for-a-phone) |
| `--approvals` | The approvals inbox only (`/v1/approvals`) | [monitoring.md](monitoring.md#the-approvals-inbox) |
| `--expire-approvals` | Requests past their time limit answer themselves (`on_expire`) | [monitoring.md](monitoring.md#requests-that-expire) |
| `--push CONTACT` | Push notifications when a run fails or waits for a person. Uses the store | [monitoring.md](monitoring.md#notifications) |
| `--channel telegram\|whatsapp\|slack\|teams` | Customers talk to the agent on Telegram, WhatsApp, Slack or Teams (`graphrt serve --channel KIND=<agent>`). Repeatable, each kind once. Uses the store | [channels.md](channels.md) |

```sh
graphrt deploy --target fly --direct --deployment support-bot --version v2 --region ams \
  --secret OPENAI_API_KEY \
  --chat --chat-access guests --cors-origin https://www.example.com \
  --monitor --push mailto:you@example.com --expire-approvals support.grt
# deployed v2 to https://support-bot.fly.dev (POST https://support-bot.fly.dev/v1/workflows/support/runs)
# chat page: https://support-bot.fly.dev/chat/
# monitoring page: https://support-bot.fly.dev/monitor/
# store: Fly volume vol_4y5… (1 GB in ams, billed by Fly; in use)
```

`--chat-access users` is not offered: it needs an identity provider
(`--verify-jwt`), which a direct deploy does not set up. Options are checked
before any call to Fly: a chat option without `--chat`, a `--cors-origin`
that is not `scheme://host[:port]` (or `*`), or a `--push` contact that is
not `mailto:` or `https:` is `invalid_input`. Pairing links name the
deployment's own address (`--public-url https://<app>.fly.dev` is set for you).

The deploy result says what the version serves:

```json
"serve": {"chat": "guests", "cors_origins": ["https://www.example.com"], "conversations": false,
          "monitor": true, "devices": true, "approvals": true, "expire_approvals": true, "push": true},
"store": {"volume": "vol_4y5…", "size_gb": 1, "region": "ams", "in_use": true}
```

`chat` is `key`, `guests` or `null`. `devices` and `approvals` are also true
when `--monitor` turned them on. The push contact is not echoed. `channels`
lists the messaging channels (`["telegram", "whatsapp"]`, `[]` without any).

### Telegram, WhatsApp, Slack and Teams

`--channel KIND` serves the agent on a messaging channel, with the same
behaviour as [`graphrt serve --channel`](channels.md). The channel's
credentials are ordinary `--secret`s, so they become Fly secrets and never
appear in the Machine command, the deploy output or the logs:

| Channel | Secrets | Also |
| --- | --- | --- |
| `telegram` | `TELEGRAM_BOT_TOKEN` | Needs `--mode always-on`: the server asks Telegram for messages (long polling), which a stopped Machine cannot do |
| `whatsapp` | `WHATSAPP_ACCESS_TOKEN`, `WHATSAPP_PHONE_NUMBER_ID`, `WHATSAPP_APP_SECRET`, `WHATSAPP_VERIFY_TOKEN` | Set the callback URL `{url}/v1/channels/whatsapp/webhook` in the Meta app; Meta's request starts a stopped Machine |
| `slack` | `SLACK_BOT_TOKEN`, `SLACK_SIGNING_SECRET` | Set the Request URL of Event Subscriptions and Interactivity to `{url}/v1/channels/slack/webhook`. With `SLACK_APP_TOKEN` instead it uses Socket Mode, which needs `--mode always-on` ([slack-teams.md](slack-teams.md)) |
| `teams` | `TEAMS_APP_ID`, `TEAMS_APP_PASSWORD`, `TEAMS_TENANT_ID` | Set the Messaging endpoint of the Azure Bot to `{url}/v1/channels/teams/webhook` |

```sh
export WHATSAPP_ACCESS_TOKEN=... WHATSAPP_PHONE_NUMBER_ID=... WHATSAPP_APP_SECRET=... WHATSAPP_VERIFY_TOKEN=...
graphrt deploy --target fly --direct --deployment support-bot --version v3 --region sin \
  --secret OPENAI_API_KEY --secret WHATSAPP_ACCESS_TOKEN --secret WHATSAPP_PHONE_NUMBER_ID \
  --secret WHATSAPP_APP_SECRET --secret WHATSAPP_VERIFY_TOKEN --channel whatsapp support.grt
```

Fly keeps secrets between deploys: a later deploy names `--channel whatsapp`
again and may leave out the `--secret`s that did not change. Channel
conversations live in the store, so a redelivered message does not run twice
after a restart. A team member answers a customer through the server's
[`/v1/channels` routes](channels.md#a-person-takes-over).

### The store volume

Paired devices, push subscriptions with the server's push key, and
conversations must outlive a Machine that stops when idle and a redeploy. A
deploy with `--monitor`, `--devices`, `--push`, `--conversations` or `--channel` therefore
gets **one Fly volume** (`graphrt_store`, 1 GB, encrypted) mounted at `/data`
as `graphrt serve --store /data`. Fly bills a volume while it exists, also
while the Machine is stopped: about **0.15 USD per month** for 1 GB at Fly's
list price. A deploy with none of these creates no volume and costs
nothing extra: a chat page for guests keeps nothing on the server (guest
sessions are signed tokens), and neither does the approvals inbox.

With a store, the run records are on the volume too, so runs stay listed
across stops. Finished runs older than 30 days are deleted
(`--store-ttl-days 30`).

Fly attaches a volume to a Machine only when the Machine is created. A
deployment with a store therefore keeps **one Machine**, named
`graphrt-store`, and what changes for it:

- **A deploy updates that Machine in place.** There is a restart of a few
  seconds instead of the blue/green overlap, and the volume stays where it is.
  The store needs `--region` on the deploy that creates it. Later deploys run
  in the volume's region whatever `--region` says.
- **A version that does not get healthy is undone.** The Machine gets the
  config it served with before and is started again. The error says so.
- **No standby versions.** `list-versions` shows the serving version only, and
  `rollback` has nothing to go back to: deploy the earlier agent file again.
- **The server runs as root inside the Machine** (a microVM of its own),
  because a new volume belongs to root and the image's default user could not
  write to it.

Turning the store's features **on** for a deployment without one creates the
`graphrt-store` Machine next to the existing version, which becomes a standby.

### Large agents and their knowledge

Fly refuses a Machine request of about 2 MiB, and the agent file travels
inside it. An agent file larger than **1 MiB** (typically a `.grt` bundle with
its knowledge base: passages, vectors and chunking settings) is therefore kept
on the [store volume](#the-store-volume) and served from there. There is no
size limit besides the volume and the Machine's memory:

1. The deployment gets the `graphrt-store` Machine and its volume, like a
   deploy with a store (so `--region` is needed the first time). The volume
   is sized for the agent: 3 times its size plus 1 GB, at least 1 GB, and is
   grown (never shrunk) when a larger agent arrives.
2. The agent is uploaded onto the volume through the Machines API `exec`, in
   pieces of 2 MiB, by `graphrt internal store-file` inside the Machine (the
   image has no shell). A piece carries its offset, so a retried call never
   writes twice, and the file is only used once its SHA-256 matches the one
   computed before the upload. A new Machine boots without services and with a
   placeholder agent while it receives the file. The `starting` step reports
   `Uploading the agent onto the store volume: 12.0 of 52.4 MB`.
3. Each agent lives in its own directory, `/data/agents/<first 16 hex digits
   of its SHA-256>/`, which `graphrt serve` serves. The version serving
   during the upload keeps its file, so a version that does not get healthy is
   undone as for any store deploy. A redeploy of the same file uploads
   nothing. After the new version is live, older agents are deleted.
4. The Machine gets more memory for a large agent, since knowledge stores are
   held in memory: 256 MB plus 4 MB per MB the agent takes once unpacked (a
   bundle's knowledge store compresses well), rounded up to 512 MB, 1 GB,
   2 GB, 4 GB or 8 GB (2 GB per shared CPU).

`--drop-store` is refused for such an agent: the volume holds it. The
Machine's image must come from this graphrt version or a newer one (the
default `--image` does); an older image has no `graphrt internal store-file`
and the deploy fails at the upload with a message saying so.

#### Turning the store off and deleting it

Two steps, so that turning a feature off never loses data by accident:

1. **Turn the features off**: deploy without `--monitor`, `--devices`,
   `--push` and `--conversations`. The new version runs without the volume.
   The volume and the `graphrt-store` Machine stay, stopped: the result
   reports `"store": {…, "in_use": false}` and the `checking` step says
   `store volume vol_… is kept and still billed`. Turning the features on again
   finds the same devices, subscriptions and push key.
2. **Delete the volume**: deploy once more with `--drop-store` (the same
   agent file, and the same `--version` label if you do not want a new
   version). It destroys the `graphrt-store` Machine, then the volume, with
   every paired device, push subscription, conversation and stored run on
   it. The result reports `"store": null` and Fly stops billing the volume.

```sh
# 1. off: the volume is kept (and billed)
graphrt deploy --target fly --direct --deployment support-bot --version v3 --region ams support.grt
# 2. delete it
graphrt deploy --target fly --direct --deployment support-bot --version v3 --drop-store support.grt
```

`--drop-store` can also be given on the deploy that turns the features off:
the new version starts first, then the volume goes. It is refused together
with a flag that uses the store, and is not an error when there is no volume.
A device paired before has to be paired again, and browsers have to turn
notifications on again. `graphrt deploy remove` deletes the app with its
volume.

Fly takes a daily snapshot of every volume and keeps it for 5 days (Fly bills
snapshots by the data stored, the first 10 GB a month are free). Fly can
restore a deleted volume from its snapshots until they expire, so data
deleted with `--drop-store` or `remove` is gone from graphrt at once and from
Fly after at most 5 days.

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

Without [a store volume](#the-store-volume), the run store of a direct deploy
is **in memory**: runs are listed until the Machine stops. With scale-to-zero that is a few minutes after the last
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
- One Machine per version; no horizontal scaling or custom domains yet (add
  them with `fly` on the same app). The only volume is
  [the store](#the-store-volume).
- Fly limits how many Machines an organization may have: **3 on Fly's Free
  plan**, counted over all its apps. Each kept version is a stopped Machine,
  so when Fly refuses a new version's Machine for that reason, the deploy
  removes this deployment's oldest standby versions until it fits and names
  them in its `starting` step. The serving version and the `graphrt-store`
  Machine are never removed. When nothing is left to remove the deploy fails
  with `fly_api` and says to upgrade the organization to Pay As You Go (on its
  Fly billing page) or remove apps it no longer needs. The version that
  served keeps serving.
- `<app>.fly.dev` can take a minute to resolve after the IPs of a new app
  are allocated; the deploy reports success once the Machine's own
  `/readyz` check passes.
- Only Fly.io is a direct target so far.

## Tested on real Fly

Checked on 2026-11-27 against the real Machines API with an organization
token, from a CLI and image built from `main` (the fixes it found are in
0.6.3). Two apps were deployed in `sin` and `ams` with `examples/chatbot.yaml`
and an OpenAI key as a `--secret`:

- App creation, the IPv6 and shared IPv4 allocation (`<app>.fly.dev` answered
  within seconds of the deploy), secrets with `min_secrets_version`, the
  readiness check reported `passing` in `GET /machines/{id}`, metadata changes
  without a reboot, and `skip_launch` keeping a demoted Machine stopped.
- An app name another organization holds (`test`) gives `app_name_taken`.
- Scale to zero on a Machine created through the API: the Fly proxy stopped it
  when idle and started it on the next request (cold start to `GET /readyz`
  answering: 1.6 s).
- A deploy without the store's flags created no volume. A deploy with
  `--chat --monitor --devices --push` created the encrypted 1 GB volume
  `graphrt_store` in the Machine's region, mounted at `/data`, with
  `graphrt serve --store /data` running as root through `config.processes`.
  A paired device, its push subscription, the VAPID key and the run history
  survived a stop and start of the Machine and a redeploy of a new version.
- The chat page and the monitoring page (opened from a pairing link) over the
  app's public HTTPS address at phone width (390 px), with no horizontal
  scrolling. A guest chat got an answer from the model.
- Turning the features off kept the volume (`in_use: false`), `--drop-store`
  destroyed the `graphrt-store` Machine and deleted the volume, and
  `deploy remove` deleted an app together with its volume.
- `pause` and an immediate `resume`, `rollback` to a standby version, a
  redeploy of the `graphrt-store` Machine while the proxy had stopped it, and
  the Machine limit of Fly's Free plan (see [Limits](#limits)). Each of these
  took 10 to 20 s.
- Large agents, checked on 2026-11-28 with an image built from this change
  and pushed to the app's `registry.fly.io` repository: a 2.2 MB `.grt`
  bundle with a 46 MB SQLite knowledge store (9,589 passages, offline `hash`
  embeddings) was uploaded onto a new 2 GB volume in two pieces while the new
  `graphrt-store` Machine had no services, then served from
  `/data/agents/<sha>/` on a 512 MB Machine. A retrieval run over the public
  address found a sentence that is only in the bundled passages. A redeploy of
  the same file uploaded nothing; a changed bundle was uploaded next to the
  serving one, served after its readiness check, and the older agent was
  deleted from the volume (its SHA-256 on the volume matched the local file).
  `deploy remove` deleted the app with its volume.

What the real API does that the first fake did not, and graphrt now handles:
updating a stopped Machine leaves it stopped (graphrt starts it), a Machine
shows its previous config in state `replacing` for a few seconds after an
update (graphrt waits until the update is applied), and an organization at
its Machine limit refuses a new Machine (graphrt removes the oldest standby
versions). The fake Machines API of the tests (`graphrt-provider-fly`, feature
`fake`) now behaves the same way.

Not checked: which operations an app deploy token
(`fly tokens create deploy -a <app>`) allows. The table in
[Least-privilege tokens](#least-privilege-tokens) is what Fly documents for
it. The organization token is the one that was tested.
