# Monitoring and approving from a phone

`graphrt serve` can be watched from a phone or another computer: which runs
are going, what each did, which ones wait for a person, and a tap to answer.
Everything runs on your server. Each piece is off until you ask for it.

| You want | Start `graphrt serve` with | Then |
| --- | --- | --- |
| A page with runs and approvals | `--monitor` | Open `/monitor/`, enter the API key once |
| The page on your phone | the above | In Settings, pair a device and scan the code with the phone |
| A notification when a run fails or waits | `--push mailto:you@example.com` | In Settings on the phone, turn on notifications |
| Requests that answer themselves after a time limit | `--expire-approvals` | Set `expire_after_ms` and `on_expire` on the `human-input` node |
| Only the API, for your own app | `--devices`, `--approvals`, `--push ...` | Use the routes below or the [browser client](web.md#the-browser-client) |

```sh
GRAPHRT_API_KEY=... graphrt serve flows/ --store ./runs \
  --monitor --push mailto:you@example.com --public-url https://agent.example.com
```

Every flag in this table can also be set as an environment variable
(`GRAPHRT_MONITOR=1`, `GRAPHRT_PUSH=mailto:you@example.com`,
`GRAPHRT_EXPIRE_APPROVALS=1`, `GRAPHRT_PUBLIC_URL=https://agent.example.com`),
for a container without a command line of its own: see
[serving.md](serving.md#flags-as-environment-variables).

The cargo features are `devices`, `approvals`, `web-push` (with `notify`) and
`web-monitor`. They are in the default build and in the released binaries. A
build without them (`--no-default-features --features serve,llm,...`) has none
of this code.

## The monitoring page

`--monitor` serves a page at `/monitor/` and turns on `--devices` and
`--approvals`:

- **Runs**: the newest first, with their state. Filter by running, waiting,
  failed or done.
- **One run**: its answer, the [UI blocks](workflow-format.md#ui-blocks) of
  its outputs, and what it did step by step. A run that still executes is
  followed live. The owner can stop it.
- **Approvals**: every run that waits for a person, across workflows, with
  the question as a form or the tool calls an agent asks to make. Answer in
  place.
- **Settings**: notifications, paired devices, sign out.

It works at phone width, in light and dark, and below a path prefix of a
reverse proxy (`https://example.com/agent/monitor/`).

**Installing it.** The page is a web app. In Chrome or Edge use "Install app"
or "Add to Home screen". On an iPhone or iPad open it in Safari, tap Share,
then "Add to Home Screen". Installed, it opens without the browser's bars, and
on iOS that is also what makes notifications possible.

**Signing in.** The page holds nothing by itself. It asks for the API key (the
owner) or takes a device token from a pairing link (a phone). The key stays in
the page's memory, or in the browser's storage when "Remember on this device"
is ticked. A paired device is always remembered, until it is unpaired or
removed.

The page's `Content-Security-Policy` allows scripts, workers and connections
of its own origin only, and forbids framing.

## Devices: a token for a phone

The API key reads and changes everything, so it should not live on a phone. A
device token is a second kind of credential for that (`--devices`, on with
`--monitor`):

- **Scoped.** It lists and reads runs (`runs:read`) and lists and answers what
  waits for a person (`approvals:resolve`). Nothing else: it starts no run,
  stops none, deletes nothing, reads no knowledge, pairs no device.
- **Revocable.** Each token belongs to a device you named. Removing the device
  ends the token at once, on every server that shares the store directory.
- **Stored hashed.** The server keeps the SHA-256 of the token's 256 random
  bits in `<store>/devices.json` (owner only). The token is shown once, when
  the device is paired. Without `--store` devices are kept in memory and gone
  at a restart.

**Pairing in the page.** Signed in with the API key, open Settings, name the
device and press Pair. The page shows a QR code and a link. Scan the code with
the phone's camera (or open the link there): the monitoring page opens already
paired. The link is the device's key and is shown only then.

**Pairing with the API.**

```sh
curl -s https://agent.example.com/v1/devices -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' -d '{"name": "Ana'\''s phone", "expires_in_days": 90}'
```

```json
{
  "device": { "id": "dev-5c1f...", "name": "Ana's phone", "scopes": ["runs:read", "approvals:resolve"], "created_ms": 1795750000000, "expires_ms": 1803526000000, "last_used_ms": null },
  "token": "grd1.dev-5c1f....q0Zk...",
  "pairing": { "v": 1, "url": "https://agent.example.com", "token": "grd1.dev-5c1f....q0Zk..." },
  "link": "https://agent.example.com/monitor/#pair=grd1.dev-5c1f....q0Zk..."
}
```

`pairing` is what a device needs to reach the server: a host application shows
it as a QR code. `link` opens the monitoring page with the token in the URL
fragment, which browsers never send to a server. The address is `url` of the
request, else `--public-url` (or `$GRAPHRT_PUBLIC_URL`), else the `Host` the
request came to.

| Request | Who | Answer |
| --- | --- | --- |
| `POST /v1/devices` `{name, scopes?, expires_in_days?, url?}` | API key | `201`: the device, its token and the pairing payload |
| `GET /v1/devices` | API key | `{devices}`, without tokens |
| `DELETE /v1/devices/{id}` | API key | `204`. The token stops working at once |
| `GET /v1/devices/me` | device token | The device of the token |
| `DELETE /v1/devices/me` | device token | `204`. The device unpairs itself |

A server keeps at most 100 devices. `scopes` narrows a token (a wall display
that only reads: `["runs:read"]`). Without `expires_in_days` a token lives
until its device is removed.

What a device token may call:

| Endpoint | With a device token |
| --- | --- |
| `GET /v1/runs`, `GET /v1/runs/{id}`, `GET /v1/runs/{id}/events` | With `runs:read`: every run, with every event |
| `GET /v1/approvals`, `POST /v1/approvals/{run_id}` | With `approvals:resolve` |
| `/v1/push/...`, `/v1/devices/me` | Yes, for itself |
| Everything else | Refused: `401` where only the API key counts, `403` for stopping or resuming a run directly |

## The approvals inbox

`--approvals` (on with `--monitor`) lists every run that waits for a person,
whatever its workflow:

```sh
curl -s https://agent.example.com/v1/approvals -H "Authorization: Bearer $TOKEN"
```

```json
{
  "approvals": [
    {
      "run_id": "run-18f2...",
      "workflow": "refunds",
      "paused_ms": 1795750000000,
      "user_id": null,
      "session_id": null,
      "requests": [
        {
          "key": "ask",
          "node_id": "ask",
          "kind": "human_input",
          "data": { "prompt": "Refund 40 EUR to Ana?", "fields": [], "actions": [{ "id": "approve", "title": "Approve" }, { "id": "reject", "title": "Reject" }], "timeout_ms": 86400000, "on_expire": { "kind": "answer", "action": "reject" } },
          "expires_ms": 1795836400000
        }
      ]
    }
  ],
  "next_cursor": null,
  "expiry_enforced": false
}
```

A request is one node that waits: a [`human-input`](workflow-format.md#human-input-branch-pauses)
node (`kind: human_input`, with its prompt, fields and actions), or an agent
whose tool needs approval (`kind: tool_approval`, with the `calls`). Runs
paused by a shutdown drain wait for no person and are not listed.

Answer with the request's `key`:

```sh
curl -s https://agent.example.com/v1/approvals/run-18f2... -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"inputs": {"ask": {"action": "approve", "values": {"note": "ok"}}}}'
# 202 {"run_id": "run-18f2...", "status": "running", "first_event": 7, ...}
```

For a tool approval the input is `{"approved": true}` (or `false`, or
`{"approvals": {"<call id>": true}}`). The run continues in the background.
Follow it from `first_event`.

| Request | Answer |
| --- | --- |
| `GET /v1/approvals?workflow=&limit=&cursor=` | `{approvals, next_cursor, expiry_enforced}`, most recently paused first |
| `POST /v1/approvals/{run_id}` `{inputs}` | `202`. `409` when the run waits no more, `400` for an answer under a key nothing waits for |

Both take the API key or a device token with `approvals:resolve`. Resolving is
`POST /v1/runs/{id}/resume` with `async: true` and the stored snapshot: exactly
one answer wins, a second one gets `409`. A resolve can never send a snapshot
or secrets.

### Requests that expire

A `human-input` node may set a time limit and what happens then:

```yaml
expire_after_ms: 86400000
on_expire: { action: reject }   # or `stop`, the default
```

The inbox shows the deadline as `expires_ms`. By default nothing happens when
it passes: the run waits until someone answers. With `--expire-approvals` the
server carries `on_expire` out, checking every 30 seconds:

- `{ action: reject }`: the run continues as if a person picked that action,
  with the fields' defaults as values.
- `stop`: the run ends as `aborted` with the reason
  `expired: nobody answered the request for input in time`.

A run that waits for several requests is stopped when one of the expired ones
says `stop`. Otherwise the expired ones are answered and the run goes on
waiting for the rest. An answer and an expiry never both win: whichever claims
the run first does, and the other finds nothing waiting.

`expiry_enforced` in the inbox says which of the two a server does, and the
monitoring page words each request to match: "Without an answer it is answered
with "Reject" in 3 h" on a server that expires requests, "An answer is expected
within 3 h. The run waits after that too" on one that does not.

Tool approvals have no time limit and never expire.

## Notifications

`--push CONTACT` sends a push notification to subscribed browsers when a run
**fails** or **waits for a person**. It is Web Push, the standard that
browsers and installed web apps use, so there is no vendor account and no app
store:

- The server has its own key pair (VAPID), created on first use in
  `<store>/push/vapid.pkcs8`. `CONTACT` is an address the push services may
  write to about this sender (`mailto:you@example.com` or an `https:` URL).
  Some push services refuse a sender without one.
- A browser subscribes at its vendor's push service (Google, Mozilla, Apple)
  and hands the server an endpoint and two keys. The server keeps them in
  `<store>/push/subscriptions.json`.
- Each message is encrypted for that browser (RFC 8291). The push service
  relays it and cannot read it.
- Requests to push services go through the server's egress policy: `https`
  only, no private addresses, no redirects. A subscription whose push service
  answers `404` or `410` is removed.

A notification names the workflow and what is asked or what went wrong, in at
most 180 characters. Tapping it opens the run in the monitoring page. A run
that succeeds or is stopped says nothing. With `--user-resume`, a question to
the user who started the run is not announced either: they are looking at it.

**Turning it on.** In the monitoring page: Settings, "Turn on notifications",
allow the browser's question, then "Send a test".

| Platform | What it takes |
| --- | --- |
| Android (Chrome, Edge, Firefox) | Works in the browser and installed |
| Desktop (Chrome, Edge, Firefox, Safari 16+) | Works in the browser |
| iPhone and iPad (iOS 16.4+) | Only for the page added to the Home Screen, opened from there. The page says so when it sees a browser tab on iOS |

Push needs a secure origin: serve the agent over `https` (a reverse proxy or a
tunnel). `http://localhost` also counts, for trying it out.

| Request | Answer |
| --- | --- |
| `GET /v1/push` | `{public_key}`: what a browser subscribes with (`applicationServerKey`) |
| `POST /v1/push/subscriptions` `{endpoint, keys: {p256dh, auth}}` | `201`. The body is `PushSubscription.toJSON()` |
| `DELETE /v1/push/subscriptions` `{endpoint}` | `204` |
| `POST /v1/push/test` `{endpoint}` | `202`: one test notification to that browser |

All take the API key or a device token. A device removes and tests only its
own subscriptions, and removing a device removes them. A server keeps at most
500 subscriptions, 5 for each device.

### Other channels

Web Push is one implementation of a small trait in the `graphrt-cli` crate:

```rust
pub trait Notifier: Send + Sync + std::fmt::Debug {
    fn name(&self) -> &'static str;
    fn notify<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<(), NotifyError>>;
}
```

A `Notice` has the kind (`approval_needed` or `run_failed`), the run, the
workflow, a title and a body. A bridge to Telegram, Slack or Teams is another
`Notifier` handed to the same `Notifications`. None ships with graphrt yet.
[Completion callbacks](serving.md#completion-callbacks) of webhooks remain the
way to tell another system about every run.

## In your own page

The page is built from two parts of the npm package `graphrt` you can use
directly (see [web.md](web.md#the-npm-package)):

```ts
import { GraphrtClient } from "graphrt/client";

const client = new GraphrtClient({ baseUrl: "https://agent.example.com", token: deviceToken });
const { approvals } = await client.approvals.list();
for (const approval of approvals) {
  for (const request of approval.requests) {
    console.log(approval.workflow, request.kind, request.data);
  }
}
await client.approvals.resolve(runId, { ask: { action: "approve", values: {} } });
```

`client.devices` pairs, lists and revokes devices, `client.push` subscribes a
browser, and `<graphrt-monitor server="...">` of `graphrt/monitor`
(`import { defineGraphrtMonitor } from "graphrt/monitor"`) is the whole view as
one element.
