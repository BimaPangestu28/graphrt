# Chatbot: Flutter / React Native + Supabase

A multi-user AI chatbot for a mobile app, with **no secret in the app**: users
sign in with Supabase Auth, the app sends each message with the user's own
access token to `graphrt serve`, and an agent answers from that user's data
only, streaming tokens as they are generated.

| Part | Where |
| --- | --- |
| Backend: workflow, Postgres schema with RLS, container, deploy config, tests | [`examples/supabase-chatbot`](../examples/supabase-chatbot/) |
| Flutter client | [graphrt-examples/supabase-chatbot/flutter](https://github.com/BimaPangestu28/graphrt-examples/tree/main/supabase-chatbot/flutter) |
| React Native (Expo) client | [graphrt-examples/supabase-chatbot/react-native](https://github.com/BimaPangestu28/graphrt-examples/tree/main/supabase-chatbot/react-native) |
| Supabase plugin (`user_rpc` mode) | [graphrt-plugins/supabase](https://github.com/BimaPangestu28/graphrt-plugins/tree/main/supabase) |

## Architecture

```text
 Flutter / React Native app                graphrt serve                         Supabase
 ─────────────────────────────             ─────────────                         ────────
 Supabase Auth sign-in ───────────────────────────────────────────────────────►  Auth: session (JWT)
 holds: project URL, publishable key,
        the user's session — nothing else

 POST /v1/workflows/agent/runs  ─────────►  1 verify JWT (JWKS / HS256,
 X-User-Authorization: Bearer <jwt>           exp, iss, aud, sub)        ─ 401
 {"stream":true, inputs.message,            2 rate limit per `sub`      ─ 429
  sys.history, sys.today}                   3 agent (model key stays here)
                                              └─ supabase/call_function ───────►  POST /rest/v1/rpc/<allowlisted>
                                                 JWT in tool_context,              apikey: publishable
                                                 never shown to the model          Authorization: Bearer <user jwt>
 ◄── SSE: response {text} … run_succeeded                                          SECURITY INVOKER fn → RLS

 own history: rpc start_chat_session /
 add_chat_message / list_chat_messages ──────────────────────────────────────►  same functions, same RLS
```

## Flow of one message

1. The app reads the current access token from its Supabase session
   (refreshing it when it is about to expire).
2. It posts the message, the earlier turns (`sys.history`, at most 16 messages)
   and the local date to `graphrt serve`, with the token in
   `X-User-Authorization`. See the
   [client contract](../examples/supabase-chatbot/README.md#client-contract).
3. `serve` verifies the token in memory (+7 µs for HS256, +44 µs for ES256), applies the
   per-user rate limit, sets `sys.user_id` to the token's `sub` and starts the
   run. The token becomes a secret start input: masked in events, logs and the
   run store, never in a snapshot.
4. The agent decides whether it needs data. If so, it calls `call_function`
   with one of six allowlisted function names. The plugin sends the RPC with the
   publishable key as `apikey` and the **user's** token as `Bearer`, taken from
   the call context (`tool_context`), not from anything the model wrote.
5. PostgREST verifies the token again and runs the `SECURITY INVOKER` function
   as `authenticated`; RLS returns only that user's rows.
6. The answer streams back as `response` events, token by token. On `401` the
   app refreshes the session and retries once; on `429` it waits `Retry-After`.

## Security model

Every boundary that matters is enforced outside the model:

| Layer | Enforces | Stops |
| --- | --- | --- |
| App | Holds only the project URL, the publishable key and the user's session | Leaked LLM, service role or serve keys (there are none to leak) |
| `serve --verify-jwt` | Signature, `exp`, `iss`, `aud`, `sub`; a user token opens only run starts of workflows that take it and feedback on its own runs ([rules](serving.md#what-a-user-token-may-do)) | Forged or expired tokens, reading other users' runs (`/v1/runs` and events need the admin key) |
| `serve --user-rate-limit` | Runs per minute per `sub` | One user exhausting the model budget |
| `tool_context` + `type: secret` | The JWT reaches the plugin, never the model's context, events, logs or store | The model revealing the token, even when told to |
| Plugin `user_rpc` | Only `call_function`, only names in `SUPABASE_TOOL_RPC_ALLOWLIST`, only as the user; refuses service role keys | Prompt injection calling `exec_sql`, table endpoints or admin functions |
| Postgres | `SECURITY INVOKER` functions, RLS on `auth.uid()`, `EXECUTE` for `authenticated` only | User A reading or writing B's rows, whatever arguments the model passes |
| Prompt | Treat data as data, refuse other users' data | Most misbehaviour, cheaply. **Not** a boundary: the client controls its own `sys.history` |

What a hostile user (or a prompt injection in their own stored data) can still
do: steer their own conversation and call the six functions with any
arguments, as themselves. That is why the allowlist must only contain
functions that are safe for any signed-in user with any arguments.

**Tested end to end** by
[`examples/supabase-chatbot/tests`](../examples/supabase-chatbot/tests/README.md):
a scripted hostile model, signed in as user A, tries to read and write user B's
rows, obeys an injected instruction to call `exec_sql` (present in the
database, not allowlisted) and other tools, and tries to reveal the JWT. The
test runs real `graphrt serve`, the real plugin, real PostgREST and Postgres,
and checks the SSE stream, run store, logs, model input and database for B's
data and for the token. A canary run with a wrongly configured allowlist must
leak, so the test is known to catch a leak.

## Setup

1. **Database.** Apply
   [`supabase/migrations`](../examples/supabase-chatbot/supabase/migrations/)
   (`supabase db push`, or paste into the SQL editor) and check it with
   `supabase/tests/isolation.sql` (prints `isolation: ok`).
2. **Plugin.** `graphrt plugin install https://github.com/BimaPangestu28/graphrt-plugins --rev main --subdir supabase`
3. **Server config.** Copy
   [`.env.example`](../examples/supabase-chatbot/.env.example) to `.env`: the
   model key, `GRAPHRT_API_KEY`, the Supabase URL and **publishable** key,
   JWKS URL, issuer, audience, `SUPABASE_TOOL_MODE=user_rpc` and the allowlist.
4. **Run.**

   ```sh
   set -a; . examples/supabase-chatbot/.env; set +a
   graphrt serve examples/supabase-chatbot/agent.yaml --addr 127.0.0.1:8080 \
     --trust-plugin supabase \
     --input-header user_jwt=X-User-Authorization \
     --verify-jwt user_jwt --user-rate-limit 20 --max-body-bytes 65536
   ```

5. **Client.** In [graphrt-examples](https://github.com/BimaPangestu28/graphrt-examples),
   configure `SUPABASE_URL`, the publishable (anon) key and `SERVE_URL`
   (Flutter: `config.json` + `--dart-define-from-file`; React Native:
   `.env.local`). Both clients refuse to start with a secret or service role key.
6. **Test offline** (no keys, no Supabase project):
   `examples/supabase-chatbot/tests/run.sh` (needs Postgres and PostgREST for
   the security part; see its README).

## Deploy

The [container](../examples/supabase-chatbot/README.md#container) is a static
musl `graphrt` with only `llm,serve,jwt,plugins` plus the plugin on distroless
Node.js; every secret is an environment variable. Ready-made notes for
[Fly.io, Cloud Run and a VPS behind Caddy](../examples/supabase-chatbot/README.md#deploy-notes).
Keep:

- `GRAPHRT_API_KEY` set (the server refuses `--verify-jwt` without it, on
  every address);
- SSE unbuffered at the proxy (`flush_interval -1` in Caddy,
  `proxy_buffering off` in nginx) and the request timeout above the agent's
  `timeout_ms`;
- the rate limit in mind: it is per process, so N instances allow N times the
  limit (put a global one at the edge if that matters).

## Latency

Built to keep a chat turn as fast as the model allows:

- In-process JWT check and rate limit: no auth proxy hop, no network call per
  request (the JWKS is cached and refreshed in the background).
- Tokens stream from the model to the app as they are generated (~6 µs from
  the model's first token to the stream).
- At most 3 tool turns (`max_iterations: 4`), parallel tool calls, row caps and
  `max_tool_output_chars` keep later turns small; `max_tokens: 600` keeps
  answers short.
- After an early `401`/`429`, `serve` drains the small unread request body so
  the app's keep-alive connection stays usable (no reconnect, no "Broken pipe"
  on the retry).

Measured with mock upstreams: first message 0.2 s including the plugin
process start, then ~5 ms per message excluding the model.
