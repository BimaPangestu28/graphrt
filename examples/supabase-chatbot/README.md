# Supabase chatbot (backend)

Guide: [docs/chatbot-supabase.md](../../docs/chatbot-supabase.md) (architecture, flow, security model, setup, deploy).

## Overview

A multi-user chatbot backend: a mobile or web app signs users in with
[Supabase Auth](https://supabase.com/docs/guides/auth) and chats with an
`agent` on `graphrt serve`. The agent reads and writes the user's data only
through a few allowlisted Postgres functions, called **as that user** (their
JWT), so row level security decides what it can see.

This folder is the backend only. The Flutter and React Native clients live in
[graphrt-examples](https://github.com/Graphrt/graphrt-examples)
(`supabase-chatbot/flutter`, `supabase-chatbot/react-native`) and follow the
[client contract](#client-contract) below.

```text
 app (Supabase Auth session)          graphrt serve (this folder)                  Supabase
 ───────────────────────────          ───────────────────────────                  ────────
 POST /v1/workflows/agent/runs ──►  verify JWT (JWKS, iss, aud, exp)  ─ 401
 X-User-Authorization: Bearer <jwt>  rate limit per `sub`             ─ 429
 {inputs.message, sys.history}       agent (OpenAI key stays here)
                                       └─ supabase/call_function ──────────────►  POST /rest/v1/rpc/<allowlisted>
 ◄── SSE: response tokens              (user JWT via tool_context,                 apikey: publishable
                                        never visible to the model)                Authorization: Bearer <user jwt>
                                                                                   → SECURITY INVOKER fn + RLS
```

| File | What |
| --- | --- |
| [`agent.yaml`](agent.yaml) | The workflow: start inputs, the agent with the supabase tool, memory from `sys.history`, answer |
| [`supabase/migrations/`](supabase/migrations/20261120000000_chatbot.sql) | Sample tables with RLS on `auth.uid()`, the six bot functions (`SECURITY INVOKER`, row caps, `EXECUTE` for `authenticated` only), chat sessions and messages |
| [`supabase/tests/isolation.sql`](supabase/tests/isolation.sql) | User A cannot read or change user B's rows, through tables or functions; anon cannot call anything |
| [`Dockerfile`](Dockerfile) | Small image: static graphrt + the plugin on distroless Node.js |
| [`deploy/fly.toml`](deploy/fly.toml), [`.env.example`](.env.example) | Deployment config; every secret is an environment variable |
| [`tests/`](tests/README.md) | Offline tests, one command (`tests/run.sh`): the smoke test with mock upstreams, and the end-to-end security test (real PostgREST + Postgres, a scripted hostile model) that user A cannot get user B's data and prompt injection cannot leave the allowlist or reveal the JWT |

## Features

- **No secret in the app.** The app holds only the Supabase URL, the
  publishable key and the user's session. The LLM key, the serve API key and
  everything else stay on the server.
- **JWT verified inside `graphrt serve`** (`--verify-jwt`): signature against
  the project's JWKS (ES256/RS256, or the legacy HS256 secret), `exp`, `iss`,
  `aud`, non-empty `sub`. A valid user token is enough to start a chat run of a
  workflow that takes `user_jwt`; it opens nothing else.
- **Per-user rate limit** keyed by the JWT `sub` (`--user-rate-limit`).
- **The model never sees the token.** It reaches the plugin in the tool call
  context (`tool_context`), is masked in events, logs and the run store, and is
  redacted from tool results.
- **Defense in depth against prompt injection:** the plugin runs in `user_rpc`
  mode (only `call_function`, only allowlisted names, only as the user), the
  functions are narrow and `SECURITY INVOKER`, RLS isolates users, and the
  system prompt tells the model to treat data as data.
- **Low latency:** at most 3 tool turns per message, small outputs, tokens
  streamed as they are generated, verification in memory (no network call per
  request).

## Prerequisites

- graphrt with the `llm`, `serve`, `jwt` and `plugins` features (the default
  build has them), or Docker for the [image](#container)
- Node.js 20+ for the [supabase plugin](https://github.com/Graphrt/graphrt-plugins/tree/main/supabase)
  (version with `SUPABASE_TOOL_MODE=user_rpc`)
- A Supabase project (or `supabase start` locally) and an OpenAI API key

## Installation

1. Apply the schema: `supabase db push` with
   [`supabase/migrations`](supabase/migrations) in your project's
   `supabase/migrations` (or paste the file into the SQL editor). Check it:

   ```sh
   psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f supabase/tests/isolation.sql   # prints "isolation: ok"
   ```

   Without the Supabase CLI, the same check runs on plain Postgres with
   [`plain-postgres-shim.sql`](supabase/tests/plain-postgres-shim.sql) (see its header).

2. Install the plugin:

   ```sh
   graphrt plugin install https://github.com/Graphrt/graphrt-plugins --rev main --subdir supabase
   ```

3. Copy [`.env.example`](.env.example) to `.env` and fill it in.

## Quick start

```sh
set -a; . examples/supabase-chatbot/.env; set +a
graphrt serve examples/supabase-chatbot/agent.yaml --addr 127.0.0.1:8080 \
  --trust-plugin supabase \
  --input-header user_jwt=X-User-Authorization \
  --verify-jwt user_jwt --user-rate-limit 20 --max-body-bytes 65536
```

`--jwt-jwks-url`, `--jwt-issuer` and `--jwt-audience` come from
`GRAPHRT_JWT_JWKS_URL`, `GRAPHRT_JWT_ISSUER` and `GRAPHRT_JWT_AUDIENCE`. Then,
with a user's access token (e.g. from `supabase.auth.signInWithPassword`):

```sh
curl -N http://127.0.0.1:8080/v1/workflows/agent/runs \
  -H 'Content-Type: application/json' \
  -H "X-User-Authorization: Bearer $USER_ACCESS_TOKEN" \
  -d '{"stream": true, "inputs": {"message": "What important dates do I have this month?"},
       "sys": {"history": [], "today": "2026-11-20"}}'
```

Offline checks of the whole chain (no keys, no Supabase project; see
[`tests/README.md`](tests/README.md)):

```sh
examples/supabase-chatbot/tests/run.sh "$(command -v graphrt)" ~/.graphrt/plugins/supabase
# ok: 401 without/forged token; streamed answer "Your display name is Ada." (first run 229 ms incl. plugin start, warm 5 ms, ...)
# ok: security e2e (real PostgREST + Postgres 16.14): cross-user and injection runs leaked nothing (2 runs, 650 ms); canary leak detected
```

## Client contract

Everything a client needs; nothing else is exposed to it.

**Request.** One request per user message:

```http
POST {BASE_URL}/v1/workflows/agent/runs
Content-Type: application/json
X-User-Authorization: Bearer <Supabase session access_token>

{
  "stream": true,
  "inputs": { "message": "What did I write last week?", "session_id": "3f0c…" },
  "sys": {
    "history": [
      { "role": "user", "content": "Hi" },
      { "role": "assistant", "content": "Hello! How can I help?" }
    ],
    "today": "2026-11-20"
  }
}
```

| Field | Rules |
| --- | --- |
| `X-User-Authorization` | The current Supabase access token. Do **not** send `Authorization` (that is the server's admin key, which no client has). |
| `inputs.message` | Required, 1-4000 characters: this turn's user message. |
| `inputs.session_id` | Optional (max 64 chars): the app's `chat_sessions.id`, so the agent can read or extend that session. |
| `sys.history` | Optional: earlier turns, oldest first, **without** the current message; `role` is `user` or `assistant`, `content` text. The server keeps the last 16 messages. See [the `sys.history` contract](../../docs/workflow-format.md#conversation-history-syshistory). |
| `sys.today` | Optional: the user's local date (`YYYY-MM-DD`), for "this week" questions. |
| `sys.user_id` | Set by the server from the token's `sub`; a client value is replaced. |

The whole body is at most 64 KiB (`--max-body-bytes`).

**Response.** `200` with `text/event-stream`. Append the `text` of every
`response` event; ignore other events.

```text
event: response
data: {"node_id":"answer","text":"You wrote","is_final":false}

event: response
data: {"node_id":"answer","text":" two entries…","is_final":false}

event: run_succeeded
data: {"type":"run_succeeded", …}
```

The stream ends after `run_succeeded`, `run_failed` (`data.error`,
`data.error_type`) or `run_aborted`. A rejected input (empty or too long
`message`) is a `run_failed` with `error_type: "invalid_input"` in that
stream, not an HTTP error.

**Errors** before the stream starts:

| Status | Body | Client action |
| --- | --- | --- |
| `401` | JSON `{"error": "invalid user token in …: expired"}` (or `missing … header`), `WWW-Authenticate: Bearer error="invalid_token"` | Refresh the Supabase session, retry once; sign out if it fails again |
| `429` with `Retry-After: <seconds>` | JSON `{"error": "rate limit exceeded for this user; retry later"}` | Wait `Retry-After` seconds (default limit: 20 messages per minute per user) |
| `429` without `Retry-After` | JSON `{"error": "the server is already executing … runs"}` | Server busy: retry after a short backoff |
| `400`, `413`, `415` | Plain text | Bug in the client: body not JSON, larger than 64 KiB, or missing `Content-Type: application/json` |

Persisting the conversation is up to the app (insert into `chat_messages`
with its own Supabase client, under RLS); the agent saves messages only when
the user asks it to.

## Configuration

| Variable | Where | Meaning |
| --- | --- | --- |
| `OPENAI_API_KEY` | server | The model key. Change `model` in `agent.yaml` for another [provider](../../docs/providers.md) (e.g. `anthropic` + `ANTHROPIC_API_KEY`). |
| `GRAPHRT_API_KEY` | server | Admin key for every endpoint except starting a run. **Required** with `--verify-jwt` (on every address, loopback included); no client has it. |
| `SUPABASE_URL`, `SUPABASE_KEY` | server | Project URL and **publishable** key (never `service_role` / `sb_secret_`). |
| `GRAPHRT_JWT_JWKS_URL` | server | `https://<ref>.supabase.co/auth/v1/.well-known/jwks.json` |
| `GRAPHRT_JWT_ISSUER` | server | `https://<ref>.supabase.co/auth/v1` |
| `GRAPHRT_JWT_AUDIENCE` | server | `authenticated` |
| `SUPABASE_JWT_SECRET` | server | Only for projects on the legacy HS256 secret, with `--jwt-secret-env SUPABASE_JWT_SECRET` |
| `SUPABASE_TOOL_MODE` | server | `user_rpc` |
| `SUPABASE_TOOL_RPC_ALLOWLIST` | server | `get_my_profile,list_diary_entries,list_important_dates,list_chat_messages,start_chat_session,add_chat_message` |
| `SUPABASE_TOOL_TIMEOUT_MS`, `SUPABASE_TOOL_MAX_ROWS` | server | `8000`, `50`: one slow or huge function cannot stall the chat |

Latency and cost knobs in `agent.yaml`: `max_iterations: 4` (three tool turns,
then a forced answer), `max_tool_calls_per_turn: 3`, `max_tool_output_chars:
8000`, `completion_params.max_tokens: 600`, `memory.window.size: 8`,
`timeout_ms: 45000`.

### Why the JWT check and the rate limit live in `graphrt serve`

Both are small and hot-path, so they are built in (cargo feature `jwt`, on by
default) rather than delegated to a proxy:

- No extra hop or process: verification is a signature check in memory
  (measured: +7 µs per request for HS256, +44 µs for ES256 over a ~95 µs
  request); the JWKS is fetched at startup, refreshed every 10 minutes and on
  an unknown `kid`.
- No new dependencies: `ring` and `reqwest` are already linked for TLS; the
  feature adds ~33 KiB to the binary.
- The server knows the verified `sub` and passes it to the run as
  `sys.user_id`; a proxy would have to forward it in a header the workflow
  must trust.

The rate limit is per process: with N instances a user gets up to N times the
limit. For a global limit put it at an edge you already run (Cloud Armor, a
Fly proxy, nginx `limit_req` keyed on a verified claim); for one small
instance, the built-in limit is enough.

## Security notes

- The system prompt is not a security boundary. A client controls its own
  `sys.history` (including `system` entries) and can steer its own
  conversation; what it can never do is see another user's rows, because the
  token, the allowlist, `SECURITY INVOKER` and RLS are enforced outside the
  model.
- Only add functions to the allowlist that are safe for any signed-in user to
  call with any arguments. Never add a dynamic-SQL helper such as `exec_sql`.
  A `SECURITY DEFINER` function bypasses RLS and must check `auth.uid()` itself.
- Keep `GRAPHRT_API_KEY` set: it closes `/v1/runs`, run events and the rest
  of the admin API, which would otherwise show every user's chats.
  `graphrt serve` refuses to start with `--verify-jwt` without it, on loopback
  too.

## Container

```sh
docker build -f examples/supabase-chatbot/Dockerfile -t supabase-chatbot .   # from the repo root
docker run --rm -p 8080:8080 --env-file examples/supabase-chatbot/.env supabase-chatbot
```

Multi-stage: a static musl `graphrt` with only `llm,serve,jwt,plugins` (5.4 MiB
on macOS arm64; no JS engine, MCP, RAG or WASM), the supabase plugin from a
pinned graphrt-plugins commit (`PLUGINS_REV`), on
`gcr.io/distroless/nodejs22-debian12:nonroot` (no shell, no package manager,
non-root). Node.js is needed by the plugin and is most of the image size. No
key is baked in; `HEALTHCHECK` is left to the platform (`GET /healthz`).

## Deploy notes

Nothing here deploys anything; these are starting points.

**Fly.io.** [`deploy/fly.toml`](deploy/fly.toml): set secrets with
`fly secrets set` (list in the file header), deploy with
`fly deploy --config examples/supabase-chatbot/deploy/fly.toml --dockerfile examples/supabase-chatbot/Dockerfile`.
Scales to zero; the first message after a cold start also starts the Node
plugin (about 0.2 s).

**Cloud Run.**

```sh
docker build -f examples/supabase-chatbot/Dockerfile -t REGION-docker.pkg.dev/PROJECT/apps/supabase-chatbot .
docker push REGION-docker.pkg.dev/PROJECT/apps/supabase-chatbot
gcloud run deploy supabase-chatbot --image REGION-docker.pkg.dev/PROJECT/apps/supabase-chatbot \
  --port 8080 --concurrency 32 --memory 256Mi --timeout 120 --allow-unauthenticated \
  --set-env-vars GRAPHRT_JWT_JWKS_URL=https://REF.supabase.co/auth/v1/.well-known/jwks.json,GRAPHRT_JWT_ISSUER=https://REF.supabase.co/auth/v1,SUPABASE_URL=https://REF.supabase.co \
  --set-secrets OPENAI_API_KEY=openai-key:latest,GRAPHRT_API_KEY=graphrt-admin-key:latest,SUPABASE_KEY=supabase-publishable-key:latest
```

`--allow-unauthenticated` only means Google does not check IAM; graphrt checks
the Supabase JWT. SSE works on Cloud Run; keep `--timeout` above the agent's
`timeout_ms`.

**VPS.** Run the container (or the binary with the Quick start flags) bound
to `127.0.0.1:8080` behind a TLS proxy, e.g. Caddy:

```text
chat.example.com {
  reverse_proxy 127.0.0.1:8080 {
    flush_interval -1   # stream SSE tokens immediately
  }
}
```

and keep `.env` readable by the service user only (`chmod 600`).

## License

Apache-2.0, like graphrt.
