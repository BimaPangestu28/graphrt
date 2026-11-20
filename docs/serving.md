# Serving workflows

`graphrt serve <file-or-dir>` exposes workflows over HTTP. This page covers
durable runs: starting runs in the background, following them, and resuming
paused runs by id — without the client keeping snapshots.

```sh
graphrt serve examples/ --store ./runs --store-ttl-days 30
```

## Run store

| Option | Effect |
| --- | --- |
| `--store <dir>` (or `$GRAPHRT_STORE`) | Durable directory store. Paused runs survive restarts. |
| *(neither)* | In-memory store. Runs are lost when the server stops. |
| `--store-ttl-days <n>` | Hourly cleanup of runs (any status except running) not updated for `n` days. |
| `--store-inputs` | Keep each run's inputs (secrets masked) in its record; see [Run context and evaluation data](#run-context-and-evaluation-data). |
| `--max-runs <n>` | In-memory store only: keep at most `n` runs (default 1000, `0` = unlimited). The oldest finished runs are dropped first; running and paused runs are always kept. |
| `--max-active-runs <n>` | At most `n` runs execute at once (default 256, `0` = unlimited); further starts and resumes get `429 Too Many Requests`. Paused runs do not count. |

### Small devices

A finished run with its events takes roughly 10–20 KiB in the in-memory store,
so memory grows with `--max-runs`, not with uptime. Measured with a static
musl build (`--no-default-features --features llm,http,wasm,serve`, 6.3 MiB)
on Linux arm64 under cgroup limits, serving a 3-node workflow for 37,000
requests:

| CPU | Memory limit | `--max-runs` | Steady RSS | Requests/s | p50 / p99 |
| --- | --- | --- | --- | --- | --- |
| 0.1 | 16 MiB | 100 | ~3 MiB | ~100 | 100 / 200 ms |
| 0.25 | 16 MiB | 100 | ~3.4 MiB | ~600 | 4 / 89 ms |
| 1.0 | 64 MiB | 1000 (default build) | ~21 MiB | ~4,600 | 2 / 4 ms |

At 0.1 CPU the latency is the kernel's CPU quota (10 ms per 100 ms period),
not graphrt. On devices with little memory, lower `--max-runs` (or use
`--store` on flash storage) and build only the features you need.

Directory layout:

```text
runs/
  .lock                          OS lock held around every store operation
  runs/<run_id>/record.json      status, outputs, answer, pause reasons, usage, run context
                                 (and inputs with --store-inputs)
  runs/<run_id>/events.jsonl     one event per line (capped at 5 000 per run)
  runs/<run_id>/snapshot.json    snapshot of a paused run
```

Records and snapshots are replaced atomically (temporary file, fsync, rename,
directory fsync). The lock is an OS file lock, so several server processes on
one machine may share a directory; it is not meant for network file systems.
Listing reads every record, which suits thousands of runs, not millions.

Nothing secret is written: event values are serialized with secrets masked, and
snapshots never contain secret plaintext. Secrets other than `env.*` must be
passed again on resume (`secrets` in the resume body).

### Restarts

On startup the server marks runs that were `running` as `failed` with the error
`interrupted by restart` (a run's in-flight work cannot be recovered). Paused
runs stay paused and can be resumed.

### Keep-alive after early rejections

Some responses are sent before the request body is read: `401`/`403` from the
API key or `--verify-jwt`, `429` from `--user-rate-limit` or
`--max-active-runs`, `415` without a JSON `Content-Type`, `413` past
`--max-body-bytes`. The server then reads and discards the rest of the body (up
to `--max-body-bytes`, for at most 2 s), so a client's pooled keep-alive
connection stays usable for the retry. A larger or slower body is not read: the
response carries `Connection: close` and the client opens a new connection.

## Starting runs

`POST /v1/workflows/{name}/runs`

| Body field | Meaning |
| --- | --- |
| `inputs`, `sys`, `conversation` | Run input |
| `stream: true` | Respond with server-sent events |
| `async: true` | Respond `202 {run_id, run, events}` immediately; the run continues in the background |
| *(neither)* | Wait and respond with the final JSON result |
| `metadata` | Optional string map (at most 64 entries, keys up to 256 bytes, values up to 2 KB) |
| `session_id`, `user_id` | Optional ids (1-256 bytes) |
| `tags` | Optional list of strings (at most 32, each 1-256 bytes) |

For synchronous calls (blocking or streaming), the run is aborted when the
client disconnects before it finishes. Async runs are never tied to a
connection.

### Per-user credentials from headers

A chat or mobile app often holds a credential of the signed-in user (a Supabase
or other OAuth JWT) that the workflow must use on the user's behalf, for
example as the `Authorization` of an `http-request` to PostgREST. Map a request
header to a start input with `--input-header INPUT=HEADER` (repeatable):

```sh
GRAPHRT_API_KEY=change-me graphrt serve flows/ --input-header user_jwt=X-User-Authorization
```

```yaml
# flows/chat.yaml
graph:
  nodes:
    - id: start
      type: start
      variables:
        - { variable: user_jwt, type: secret, required: true }
    - id: rpc
      type: http-request
      method: post
      url: "https://<project>.supabase.co/rest/v1/rpc/get_balance"
      headers: "apikey: {{#env.SUPABASE_ANON_KEY#}}"
      authorization: { type: api-key, config: { type: bearer, api_key: "{{#start.user_jwt#}}" } }
    # ...
```

```sh
curl -N http://localhost:8080/v1/workflows/chat/runs \
  -H 'Authorization: Bearer change-me' \
  -H "X-User-Authorization: Bearer $SUPABASE_JWT" \
  -d '{"stream": true, "sys": {"query": "What is my balance?"}}'
```

- `Authorization` stays the serve API key; pick any other header for the user
  credential (`--input-header` refuses `Authorization`).
- A leading `Bearer ` is stripped. The value becomes a secret: it shows as
  `******` in responses, SSE, events, the run store, logs and OTel, is scrubbed
  from node error messages, and is never written to snapshots.
- The mapping applies to `POST /v1/workflows/{name}/runs` (blocking, `stream`
  and `async`) and `POST /v1/runs/{id}/resume`, for every served workflow whose
  start node declares the input. Such an input must be declared
  `type: secret`; the server refuses to start otherwise. Workflows that do not
  declare it are unaffected.
- A missing or empty header for a `required` input answers `401` and starts
  nothing. Setting the input in the body (`inputs`), or in the resume
  `secrets`, answers `400`: the header is its only source.
- Snapshots hold no plaintext, so a resume must send the header again (it is
  re-supplied as the `<start>.<input>` resume secret). A resume refused for a
  missing header leaves the run paused.
- Webhooks have their own `headers:` mapping (below); `--input-header` does not
  apply to them.

### Verifying end-user JWTs

With `--input-header` alone the server passes the token through unchecked and
`Authorization` must still carry the API key, so a mobile app would have to
hold that key. `--verify-jwt INPUT` (cargo feature `jwt`, on by default) makes
the server verify the token itself, and lets a valid token start runs
**without** the API key:

```sh
GRAPHRT_API_KEY=admin-only graphrt serve flows/ \
  --input-header user_jwt=X-User-Authorization --verify-jwt user_jwt \
  --jwt-jwks-url https://<ref>.supabase.co/auth/v1/.well-known/jwks.json \
  --jwt-issuer https://<ref>.supabase.co/auth/v1 --jwt-audience authenticated \
  --user-rate-limit 20
```

```sh
curl -N http://localhost:8080/v1/workflows/chat/runs -H 'Content-Type: application/json' \
  -H "X-User-Authorization: Bearer $SUPABASE_ACCESS_TOKEN" \
  -d '{"stream": true, "inputs": {"message": "What is my balance?"}}'
```

| Flag | Meaning |
| --- | --- |
| `--verify-jwt INPUT` | Verify the token in the header mapped to `INPUT` by `--input-header` |
| `--jwt-jwks-url URL` | JWKS of the identity provider (`ES256` P-256 and `RS256` keys). Fetched at startup (a failure stops the server), refreshed every 10 minutes, and early (at most every 30 s) when a token names an unknown `kid`. `https` only, except for loopback. Env fallback: `GRAPHRT_JWT_JWKS_URL` |
| `--jwt-secret-env VAR` | Environment variable holding an `HS256` shared secret (e.g. the Supabase legacy JWT secret). Can be combined with a JWKS |
| `--jwt-issuer ISS` | Required `iss`. Env fallback: `GRAPHRT_JWT_ISSUER` |
| `--jwt-audience AUD` | Required `aud` (a string, or one entry of an array; Supabase uses `authenticated`). Env fallback: `GRAPHRT_JWT_AUDIENCE` |
| `--user-rate-limit N` | Runs per minute per verified user (`sub`), bursts of up to N; `0` (default) = unlimited |

The env fallbacks let a container without a shell take project-specific values
from its environment.

What a request gets:

- **Valid token** (signature by a configured key with the key's own `alg`;
  `exp` present and not past; `nbf`, `iss`, `aud` when set, 30 s leeway; a
  non-empty `sub`): the run starts, with or without the API key. The run's
  `sys.user_id` and its `user_id` (run record and spans) are set to `sub`,
  replacing any client value. Only `HS256`,
  `ES256` and `RS256` are accepted; `alg: none`, other algorithms and the
  legacy anon / service-role keys (no `sub`) are refused.
- **Invalid token**: `401 {"error": "invalid user token in `x-user-authorization`: expired"}`
  with `WWW-Authenticate: Bearer error="invalid_token"`, even when the API key
  is valid, so a bad token never reaches a tool.
- **No token**: the API key rules apply as before (and a required input still
  answers `401`).
- **Over the rate limit**: `429` with `Retry-After` (seconds). Refused requests
  do not use up the quota. The limit is per process; with several instances
  each one counts separately.

A user token opens only `POST /v1/workflows/{name}/runs`, and only for
workflows whose start node declares the verified input (others answer `403`,
so internal workflows served next to the chat stay key-only), plus
[feedback](#scores-and-feedback) on the user's own runs. Every other endpoint
(run lists, events, resume, abort, validate, knowledge) still needs the API key,
so users cannot read each other's runs; for the same reason the server refuses
to start with `--verify-jwt` on a non-loopback address unless the API key is
set. Verification is in memory (measured: +7 µs per request for `HS256`, +44 µs
for `ES256`). A complete example, with a Supabase schema and container, is
[examples/supabase-chatbot](../examples/supabase-chatbot/).

## Streaming (SSE)

With `stream: true` the response is `text/event-stream`: one SSE message per
run event, named after the event's `type`, with the event JSON as `data`. The
stream ends right after the terminal event (`run_succeeded`, `run_failed`,
`run_aborted` or `run_paused`).

```text
event: node_chunk
data: {"type":"node_chunk","frame":"","node_id":"agent","exec_id":"...","field":"text","chunk":"Your"}

event: response
data: {"node_id":"answer","text":"Your","is_final":false}
```

Two kinds of text arrive while a run executes:

- `node_chunk`: raw tokens of a node's streaming output (`field` is `text` or
  `reasoning`), one event per provider delta, as soon as it arrives.
- `response`: the ordered text of each `answer` node (`{node_id, text,
  is_final}`, no `type` field), sent just before the raw event that produced
  it. `is_final: true` comes once per answer node, on an empty `text`, when its
  template is fully rendered. A chat client usually only needs these.

`llm` and `agent` nodes stream per token. An `agent` streams every turn,
including turns that may still call tools; tool-call arguments never appear as
text, and text written before a tool call stays part of the answer, separated
from later text by a `"\n\n"` chunk (see
[workflow-format.md](workflow-format.md#agent); `stream_tool_turns: false`
streams only the final answer).

**Retries and handled errors.** A node with `retry.max_retries > 0` or an
`on_error` strategy other than `fail` still emits `node_chunk` per token for
every attempt, but its `response` text is held back until the node finishes, so
the answer never mixes a failed attempt with the retry (or with a
`default-value`). A `node_retry` event means the chunks of the previous
attempt are void. For a live answer from such a node, render `node_chunk`
events and reset on `node_retry`, or drop the retry policy.

### Run context and evaluation data

`metadata`, `session_id`, `user_id` and `tags` describe who ran what, for
filtering and later evaluation. They are stored on the run record (and kept
when the run is resumed) and, with [OpenTelemetry](observability.md) on, stamped
on every span of the run as `session.id`, `user.id`, `gen_ai.conversation.id`,
`langfuse.session.id`, `langfuse.user.id`, `langfuse.trace.tags` and
`langfuse.trace.metadata.<key>`. A request that exceeds the limits gets `400`.

```sh
curl -s localhost:8080/v1/workflows/support/runs -H 'content-type: application/json' -d '{
  "inputs": {"question": "Where is my order?"},
  "session_id": "chat-42", "user_id": "u-17", "tags": ["prod"], "metadata": {"tenant": "acme"}
}'
```

Run records gain these fields, all omitted when empty:

| Field | Meaning |
| --- | --- |
| `metadata`, `session_id`, `user_id`, `tags` | From the request (`user_id` is the token's `sub` when `--verify-jwt` verified one) |
| `trace_id`, `span_id` | OpenTelemetry trace id and root (`invoke_workflow`) span id of the latest execution, when tracing is on; lets later evaluation scores join the same trace |
| `inputs` | The run's inputs as the start node accepted them, with `secret`-typed inputs stored as `******`. Only with `graphrt serve --store-inputs` |

`--store-inputs` is off by default because inputs often hold personal data;
turn it on to build evaluation datasets from production runs.

## Following runs

| Request | Response |
| --- | --- |
| `GET /v1/runs?workflow=&status=&limit=&cursor=` | `{runs, next_cursor}`, newest first (max 500 per page) |
| `GET /v1/runs/{id}` | The run record, plus `active` and, for paused runs, `resume_inputs` |
| `GET /v1/runs/{id}/events` | SSE replay of stored events, then a live tail while the run executes |
| `GET /v1/runs/{id}/events?stream=false&from=N` | Stored events as a JSON array, starting at index `N` |
| `POST /v1/runs/{id}/abort` | `202` when the run is executing |
| `DELETE /v1/runs/{id}` | `204`; `409` while the run executes |

`resume_inputs` lists what each paused node waits for:

```json
{
  "status": "paused",
  "resume_inputs": [
    { "key": "review", "node_id": "review", "kind": "human_input",
      "data": { "prompt": "Approve an expense of 50 for: Lunch?", "fields": [], "actions": [] } }
  ]
}
```

`key` is what to use in the resume `inputs`: the node id, or `<frame>/<node_id>`
for a node paused inside an iteration or loop.

A listener that falls behind the live tail receives a `lagged` event with the
number of skipped events; the complete history stays available from the store.

## Scores and feedback

Each run collects evaluation **scores**: from [online evaluators](evals.md#online-evaluation)
(`--evaluators`, `source: online`) and from people (`source: human`). They are kept next to
the run (`runs/<id>/scores.json` in a `--store` directory, in memory
otherwise), so `--max-runs`, `--store-ttl-days` and `DELETE /v1/runs/{id}`
remove them with it. (Cargo feature `scores`, on by default; not in the IoT
build.)

| Request | Response |
| --- | --- |
| `GET /v1/runs/{id}/scores` | `{run_id, scores}`, oldest first; `404` for an unknown run |
| `POST /v1/runs/{id}/feedback` | `201` and the stored score; `400` for an invalid body, `404` for an unknown run, `409` once a run has 256 scores |

```sh
curl -s localhost:8080/v1/runs/$RUN/feedback -H "authorization: Bearer $GRAPHRT_API_KEY" \
  -H 'content-type: application/json' \
  -d '{"name": "thumbs", "value": true, "comment": "solved it", "user": "agent-7"}'
```

| Field | Rules |
| --- | --- |
| `name` | 1–200 bytes, no control characters: the score name |
| `value` | A number (finite), a boolean, or a text of 1–500 characters (a category) |
| `comment` | Optional, up to 4096 bytes |
| `user` | Optional, 1–256 bytes: who gives the feedback |

A stored score:

```json
{
  "id": "5d1f0c3a9b8e7d6c5b4a39281706f5e4", "name": "thumbs", "value": true,
  "comment": "solved it", "source": "human", "user": "agent-7", "created_ms": 1763640000000
}
```

Online scores also carry `label` (`pass`, `fail` or `unknown`), `type` (the
assertion type), `comment` (the evaluator's reason) and, for judges, `judge`
(the judge workflow, its fingerprint and verdict). Ids are deterministic (a
hash of the run, the source, the name and the user), so the same person rating
the same run again **replaces** their score, and uploads to Langfuse are
idempotent.

**Auth.** Both endpoints take the API key. With
[`--verify-jwt`](#verifying-end-user-jwts), feedback also accepts the end
user's token, for the user's **own** runs only: the run's `user_id` must be the
token's `sub` (runs started with a user token have it; runs started with the
API key have no owner and refuse user tokens with `403`), and the score's
`user` is the verified `sub`, whatever the body says. Reading scores stays an
API-key endpoint. An invalid token is `401`, and feedback with a user token counts
against that user's `--user-rate-limit` like a run start.

With [OpenTelemetry](observability.md) on, every score (online or human) also
becomes an `evaluate {name}` span under the run's root span in the run's trace,
with a `gen_ai.evaluation.result` event (`gen_ai.evaluation.name`,
`gen_ai.evaluation.score.value`, `gen_ai.evaluation.score.label`,
`gen_ai.evaluation.explanation`), and feeds the `graphrt.eval.score`
histogram. This needs the run's `trace_id` / `span_id`, which runs only have
when they were traced.

### Uploading scores to Langfuse

Langfuse does not read scores from OTLP, so `--langfuse-scores` (cargo feature
`langfuse`, on by default) also POSTs every stored score, online and human,
to `<LANGFUSE_BASE_URL>/api/public/scores`:

```sh
export LANGFUSE_PUBLIC_KEY=pk-lf-... LANGFUSE_SECRET_KEY=sk-lf-...   # LANGFUSE_BASE_URL defaults to the EU cloud
graphrt serve flows/ --otel-preset langfuse --evaluators evaluators.yaml --langfuse-scores
```

| Score | Langfuse |
| --- | --- |
| `id` | `id`: deterministic, so a retried or repeated upload overwrites instead of duplicating |
| the run's `trace_id`, `span_id` | `traceId`, `observationId` (the root observation, which v4 observation evaluators read) |
| number / boolean / text `value` | `NUMERIC` / `BOOLEAN` (0 or 1) / `CATEGORICAL` |
| `comment` | `comment` (the evaluator's reason, or the person's comment) |
| `source`, `label`, `type`, `user`, run id, workflow | `metadata` |

Credentials and URL are those of the [`langfuse` preset](observability.md#vendor-presets)
(environment or `--env-file`, never flags). Uploads go through the egress
policy: a self-hosted Langfuse on a private or loopback address needs
`--allow-private-network`. They run on one background task from a queue of
1024; a full queue drops the upload, and `408` / `429` / `5xx` / network
errors are retried four times with backoff. Runs without a trace id (served
without OpenTelemetry) are not uploaded, so pair it with `--otel-preset langfuse`.
A score may arrive before its trace; Langfuse links them when the trace does.

## Resuming

`POST /v1/runs/{id}/resume`

```json
{ "inputs": { "review": { "action": "approve", "values": { "note": "ok" } } } }
```

The server loads the snapshot it stored when the run paused. Exactly one resume
of a pause succeeds: a concurrent or repeated resume gets `409 Conflict`.
`stream` and `async` work as for new runs. Clients that keep snapshots
themselves may still send `snapshot` in the body.

## Webhooks

`graphrt serve <dir> --webhooks webhooks.yaml` lets GitHub, Stripe, Slack or
any sender of JSON webhooks start runs directly, without glue code:

```yaml
hooks:
  # POST /v1/hooks/github-push
  github-push:
    workflow: triage                       # a served workflow
    inputs:                                # input name -> JSON Pointer into the body
      repo: /repository/full_name
      ref: /ref
    headers:                               # input name -> request header
      event: X-GitHub-Event
    signature: { scheme: github, secret_env: GITHUB_WEBHOOK_SECRET }
    idempotency_header: X-GitHub-Delivery
    callback:                              # optional: POST the result when the run ends
      url: https://ci.example.com/graphrt-done
      secret_env: CALLBACK_SECRET

  # POST /v1/hooks/stripe: the whole body becomes the input `body`
  stripe:
    workflow: billing-event
    signature: { scheme: stripe, secret_file: /run/secrets/stripe_whsec }
```

`POST /v1/hooks/{hook}` answers `202 {run_id, status, duplicate, run, events}`;
follow the run with `GET /v1/runs/{id}` and `/events` as for any async run.
`GET /v1/hooks` lists the configured hooks (no secrets).

The mapping lives in the serve-side file rather than in the workflow: the
workflow stays a portable function of its declared inputs, and the
sender-specific wiring (payload shapes, secrets, callback URLs) stays with the
deployment.

| Hook field | Meaning |
| --- | --- |
| `workflow` | Served workflow to run (checked at startup). |
| `inputs` | Input name -> [JSON Pointer](https://www.rfc-editor.org/rfc/rfc6901) (`/a/b/0`, `""` for the whole body). A pointer that does not resolve leaves the input unset: the start node applies its default or fails the run for a required input. |
| `headers` | Input name -> request header; its value is passed as a string. |
| `body_input` | Pass the whole body as this input. Defaults to `body` when `inputs` is empty. |
| `signature` | `{scheme, secret_env \| secret_file, header?, prefix?, tolerance_secs?}`, see below. |
| `idempotency_header` | Header carrying the delivery's idempotency key. Default `Idempotency-Key`. |
| `callback` | `{url, secret_env \| secret_file, on?, max_attempts?, backoff_ms?, timeout_ms?}`, see below. |

Secrets come from `secret_env` (the server's environment or `--env-file`) or
`secret_file` (e.g. a mounted container secret; a trailing newline is
ignored). A hook whose secret is missing fails startup.

### Signatures

| `scheme` | Sender | Checked |
| --- | --- | --- |
| `hmac-sha256` (default) | generic | `<header>: <prefix><hex HMAC-SHA256(body)>`; `header` defaults to `X-Signature-256`, `prefix` to `sha256=`. |
| `github` | GitHub | `X-Hub-Signature-256: sha256=<hex>` |
| `stripe` | Stripe | `Stripe-Signature: t=<unix>,v1=<hex HMAC("<t>.<body>")>` (any `v1` may match) |
| `slack` | Slack | `X-Slack-Signature: v0=<hex HMAC("v0:<ts>:<body>")>` with `X-Slack-Request-Timestamp` |
| `graphrt` | another graphrt server's callbacks | `X-Graphrt-Signature: t=<unix>,v1=<hex HMAC("<t>.<body>")>` |

The MAC is computed over the raw body and compared in constant time. The
timestamped schemes reject deliveries more than `tolerance_secs` (default 300)
away from the server clock, which bounds replays. A failed check answers `401`.

A hook **without** `signature` requires the server's bearer API key like the
rest of the API; a signed hook does not (the signature authenticates the
sender). The server warns at startup about unsigned hooks when no API key is
set.

### Idempotency

When a delivery carries the idempotency header (1–256 visible ASCII
characters), the run id is derived from the hook and the key. A redelivery with
the same key answers `202` with the first run's id, its current `status` and
`duplicate: true`, and starts nothing; concurrent redeliveries start exactly one
run. Because the run store is the dedupe record, this holds across restarts
with `--store`, for as long as the run is kept (`--store-ttl-days`,
`--max-runs`, `DELETE /v1/runs/{id}`). The key is not compared with the body:
a different body under a used key also returns the first run.

### Limits and errors

Deliveries share `--max-body-bytes` (default 2 MiB) with the rest of the API:
larger bodies answer `413`. The body must be JSON (`400` otherwise); unknown
hooks answer `404`. Webhook runs count against `--max-active-runs` like API
runs, so a busy server answers `429` and the sender retries.

A server on a loopback address without an API key accepts only loopback `Host`
names (DNS rebinding protection). When you expose such a server through a
tunnel or reverse proxy that forwards its own host name, set an API key
(`GRAPHRT_API_KEY`, or the variable named by `--api-key-env`): signed hooks still authenticate by signature alone.

### Completion callbacks

With `callback`, the server POSTs the result when the run started by the
delivery ends:

```json
{ "type": "run.succeeded", "status": "succeeded", "run_id": "hook-…", "workflow": "triage",
  "hook": "github-push", "run": "/v1/runs/hook-…", "outputs": { "…": "…" }, "answer": "…",
  "usage": { "…": "…" }, "elapsed_ms": 812 }
```

`status` is `succeeded` (with `outputs`, `answer`), `failed` (`error`,
`error_type`), `aborted` (`reason`) or `paused` (`reasons`; resume with
`POST /v1/runs/{id}/resume`). `on: [failed, aborted]` limits which statuses are
sent (default: all four). Callbacks cover the execution started by the
delivery; a later resume of a paused run does not send another one.

Each request carries:

| Header | Value |
| --- | --- |
| `X-Graphrt-Signature` | `t=<unix>,v1=<hex HMAC-SHA256("<t>.<body>")>` under the callback secret |
| `X-Graphrt-Event` | `run.<status>` |
| `X-Graphrt-Delivery`, `Idempotency-Key` | `<run_id>.<status>`, the same for every retry |
| `X-Graphrt-Attempt` | `1`, `2`, … |

To verify: split the header at `,`, recompute the HMAC of `"<t>.<raw body>"`,
compare in constant time, and reject old timestamps. A graphrt server can
receive another's callbacks with a hook using `scheme: graphrt`.

Callbacks go through the same egress policy as `http-request` nodes: private,
loopback, link-local and metadata addresses are refused (a literal address at
startup, a resolved one at delivery) unless the server runs with
`--allow-private-network`, and redirects are not followed. Network errors,
timeouts, `408`, `429` and `5xx` are retried with exponential backoff
(`backoff_ms`, default 1000, doubling, at most 5 minutes apart) up to
`max_attempts` (default 5, at most 20) requests in total; other responses are
final. Each request times out after `timeout_ms` (default 10000). Callbacks are
held in memory: a restart drops pending retries.

## Knowledge base API

When the server is started with `--knowledge <config>`, the knowledge base the
workflows' `knowledge-retrieval` nodes use is also exposed under
`/v1/knowledge`, behind the same API key. Documents added here are searchable by
running workflows immediately. It is meant as the backend of a knowledge base
UI (datasets, documents, chunks, retrieval testing).

| Endpoint | Purpose |
| --- | --- |
| `GET /v1/knowledge/datasets` | `{datasets: [{id, name, description, managed, runtime, store, embedder, chunking}]}`. `managed` is false for datasets served by an external retriever (read-only); `runtime` is true for datasets created over the API; `chunking` is `{max_chars, overlap, markdown, parent_max_chars}`. |
| `GET /v1/knowledge/resources` | `{stores, embedders, rerankers}`: the names declared in the knowledge config that new datasets can use. |
| `POST /v1/knowledge/datasets` | Create a dataset: `{id, name?, description?, store, embedder, reranker?, chunking?}` naming resources from the config (201). Ids are 1–64 ASCII letters, digits, `-`, `_` or `.`. |
| `DELETE /v1/knowledge/datasets/{dataset}` | Delete a dataset created over the API together with its documents: `{dataset, deleted_chunks}`. Datasets declared in the config answer 409 (edit the config instead). |
| `GET /v1/knowledge/datasets/{dataset}/documents?limit=&after=` | Documents ordered by id: `{documents: [{document_id, title, chunks, characters, enabled, metadata}], next}`. Pass `next` as `after` for the following page; `next` is null at the end. `limit` defaults to 50, at most 500. |
| `POST /v1/knowledge/datasets/{dataset}/documents` | Add or replace a document from text: `{document_id, text, title?, metadata?}`. |
| `PUT /v1/knowledge/datasets/{dataset}/documents/{document}/file?name=&title=` | Add or replace a document from a raw file body (PDF, Office, HTML, Markdown, EPUB, CSV, e-mail, ...). `name` (e.g. `report.pdf`) and `Content-Type` select the format. Limited by `--max-upload-bytes` (default 16 MiB). |
| `GET /v1/knowledge/datasets/{dataset}/documents/{document}` | The document's chunks in order: `{document_id, chunks: [{id, title, text, characters, metadata}]}` (vectors are never sent). |
| `PATCH /v1/knowledge/datasets/{dataset}/documents/{document}` | `{enabled: false}` disables a document: it is kept (and stays disabled when re-uploaded) but no search returns it; `{enabled: true}` restores it. Listings show `enabled`. |
| `DELETE /v1/knowledge/datasets/{dataset}/documents/{document}` | `{deleted_chunks}`; 404 when the document does not exist. |
| `POST /v1/knowledge/retrieve` | Retrieval testing: `{query, datasets, top_k?, score_threshold?, filter?, weights?, rerank?}` returns `{hits, elapsed_ms}`, exactly as the node would retrieve. |

Chunking is set per dataset in the knowledge config. With `parent_max_chars`
the dataset uses parent-child retrieval: documents are split into parents of
that size and each parent into small children (`max_chars`, no overlap); the
children are embedded and searched, and a hit returns its whole parent once,
however many of its children matched. Parents are rebuilt from their children,
so nothing extra is stored.

```yaml
datasets:
  - { id: handbook, store: local, embedder: local,
      chunking: { max_chars: 400, parent_max_chars: 1600 } }
```

Datasets created over the API pick their store, embedder and reranker from the
names the knowledge config declares, so credentials and endpoints stay in the
operator's config. They are saved next to it (`kb.yaml` -> `kb.datasets.json`,
written atomically) and recreated on the next start by every command that
loads the knowledge config (`serve`, `run`, `kb`). Running workflows see a new
dataset immediately.

Adding a document with an existing id replaces it: unchanged chunks keep their
vectors (nothing is re-embedded), changed ones are embedded, and chunks the new
version no longer contains are removed. The response reports
`{document_id, report: {stored, skipped, removed}}`.

To stay light on small devices, ingestion (text extraction and embedding) runs
one request at a time (others wait their turn), and file extraction runs off
the request workers. Errors are JSON `{error}` with a matching status (400
invalid input, 404 unknown dataset or document, 409 operation not supported by
the dataset, 413 upload too large, 502 provider failure) and an
`x-graphrt-error-kind` header.

## Tenancy

The API is single-tenant: anyone holding the server's API key can see and
resume every run. Run one server (or one store directory) per tenant, or put an
authenticating proxy in front that restricts which run ids a caller may use.
Per-user credentials passed with `--input-header` are not checked by the
server; they only reach the workflow, and whatever the workflow calls with them
(PostgREST row-level security, for example) enforces them.
