# Security guide

This page describes what graphrt guarantees when it runs a workflow: the limits of
the code sandbox, the network egress policy, how secrets are masked, and the knobs
you have when you deploy it. For reporting vulnerabilities and the formal scope, see
[SECURITY.md](../SECURITY.md).

## Trust model in one paragraph

A **workflow document** is treated as untrusted input. It may come from a user,
an agent or a template gallery. graphrt assumes it can contain hostile code-node
scripts, URLs pointing at internal services, huge loops, and templates designed to
exfiltrate secrets. The **operator** (whoever runs `graphrt serve` or embeds the
crates) is trusted. That covers provider keys, `--providers` files, plugin
installation, MCP server configs, knowledge configs and the process environment.
**Plugins** you install and **MCP servers** you configure run with your
privileges. graphrt bounds and isolates them where it can, but they are not a
sandbox boundary.

## JavaScript sandbox (`code` node)

JavaScript runs in an embedded QuickJS interpreter (rquickjs). Each execution gets
a new runtime and context, so nothing carries over between runs or nodes.

| Limit | Default | Behaviour when hit |
| --- | --- | --- |
| Wall-clock time | 10 s, further capped by the node timeout / run deadline | An interrupt handler stops the script, including inside a regex (ReDoS) and inside promise jobs. A `try/catch` cannot swallow it. Error kind `timeout`, not retried. |
| Heap | 64 MiB | Allocation fails and the script gets `out of memory`. Error kind `out_of_memory`, not retried. |
| Stack | 1 MiB | A `RangeError: Maximum call stack size exceeded` that the script can catch. The engine runs on its own thread, sized from this limit plus 2 MiB of headroom, so the native stack never overflows. |
| Result size | 4 MiB of JSON | `invalid_output`. |
| Result depth | 32 levels | `invalid_output`. |
| Error message | 8 KiB | Longer messages are truncated. |

Clamping rules:

- Limits in a workflow's `limits:` block can only **lower** the operator's limits.
  They never raise them.
- The engine has floors and a ceiling. Heap is at least 1 MiB. Stack is between
  64 KiB and 16 MiB. QuickJS reads `0` as "unlimited", so a request for `0` gets
  the floor.

What the script can see:

- Only the ECMAScript standard library, plus `atob`/`btoa`, `performance.now()`
  and `queueMicrotask`.
- No `require`, no `import` (there is no module loader), no `fetch`, timers,
  `std`/`os`, `process` or environment, and no file system.
- Inputs arrive as JSON text and results leave as JSON text. No host object is
  ever shared with the script, so prototype pollution inside the script cannot
  reach host values. Keys such as `__proto__` in the result are plain data.

Secrets:

- Variables bound to a secret are passed to `main()` **in plaintext**, because
  code often needs an API key.
- A returned string that contains one of those secrets is turned back into a
  secret value. It is masked in events and records but still usable downstream.
- A script that transforms a secret (reverses it, base64-encodes it) can still
  leak it. Do not bind keys into code you do not trust.

### Python and remote sandboxes

- `--allow-python-subprocess` (`SubprocessPython`) is **not a sandbox**. It runs
  `python3 -I -S` as your user, with the following hygiene:
  - an empty environment except `PATH` (host secrets are not inherited);
  - a fresh owner-only working directory;
  - `ulimit` CPU, memory and file-size caps on Unix (memory caps are not
    available on macOS);
  - a wall-clock kill and capped output.

  The code can still read your files and open sockets. Enable it only for
  trusted workflows.
- `HttpSandbox` (feature `remote`) posts code to a sandbox service that you
  operate. The URL is operator-configured and allowed to be private. Isolation is
  that service's job.

## Network egress

Every URL that a workflow controls goes through `graphrt_http::HttpClient` and its
`EgressPolicy`. That covers the `http-request` node and file downloads
(`remote_url`, tool files).

- **Blocked by default:**
  - `0.0.0.0/8` and `::`;
  - loopback (`127/8`, `::1`);
  - RFC 1918 ranges;
  - link-local (`169.254/16`, `fe80::/10`), which includes the cloud metadata
    address `169.254.169.254`;
  - unique-local `fc00::/7`, which includes AWS `fd00:ec2::254`;
  - CGNAT `100.64/10` (Alibaba `100.100.100.200`) and Oracle `192.0.0.192`;
  - the Azure WireServer `168.63.129.16`;
  - TEST-NETs, benchmarking, multicast, `240/4` and site-local;
  - IPv6 forms that embed an IPv4 address (IPv4-mapped `::ffff:a.b.c.d`,
    IPv4-translated `::ffff:0:a.b.c.d`, IPv4-compatible, NAT64 `64:ff9b::/96`,
    6to4 `2002::/16`) are judged by the embedded address. Local-use NAT64
    `64:ff9b:1::/48` and Teredo `2001::/32` are blocked outright.
- **Literal IPs in any notation** (decimal `2130706433`, octal `0177.0.0.1`, hex,
  short `127.1`) are normalised by the URL parser before the check.
- **DNS rebinding:** host names are resolved by graphrt's own resolver. Every
  resolved address is checked, a name that mixes public and private records is
  refused, and the connection is made only to the checked addresses. There is no
  second lookup.
- **Redirects:** every hop is re-validated, capped by `max_redirects`, and
  credential headers and secret-valued headers are dropped when the origin
  changes.
- **Proxies:** `HTTP(S)_PROXY` environment variables are ignored. If you set
  `EgressPolicy::proxy`, targets are checked by name and literal IP only, and the
  proxy must enforce its own egress rules.

These clients are **outside** the policy because their URLs come from the
operator, never from a workflow:

- LLM providers (no redirects followed);
- MCP HTTP servers;
- HTTP plugins (no redirects);
- the RAG HTTP retriever, embedder, reranker and Qdrant;
- the Hugging Face model download;
- the OTel exporter (no redirects);
- the remote code sandbox.

They honour standard proxy environment variables, except the OTel exporter and
`HttpClient`. WebAssembly plugins have no network host functions.

## Secret masking

- **Typed secrets.** A value declared `type: secret` (an `env` entry, a start
  input, a conversation variable), a header passed with `serve --input-header`,
  a re-supplied resume secret, or a secret returned by a node is a
  `Value::Secret`. It serialises, displays and `Debug`-prints as `******`
  everywhere: events, SSE, the run store, logs, OTel attributes and templates
  (`Bearer {{#env.KEY#}}` renders as `Bearer ******` in anything shown).
  Snapshots never contain `env.*` and store other secrets as redaction markers,
  so resuming needs them re-supplied.
- **Free text.** Every node error message is scrubbed of the plaintext of every
  secret the run has seen, wherever it appears inside the string. This applies
  before the message reaches `node_failed`, `node_retry` and `run_failed`
  events, handled-error `error_message` outputs, the store or OTel. LLM
  provider error bodies are also scrubbed of the credentials the request
  carried (headers and `key` query parameters), which never enter the variable
  pool.
- **Limits of masking.**
  - Values shorter than 4 characters are not scrubbed from text.
  - `env` entries **without** `type: secret` are ordinary strings. Declare
    every key as `type: secret`.
  - Plaintext that a node legitimately receives can come back in a transformed
    form: an HTTP response that echoes the request, a code script that encodes
    the key, a plugin granted `permissions.secrets`. graphrt cannot recognise
    those.
  - A workflow author can probe a secret through conditions (`starts with`).
    Authors are expected to hold the secrets their workflow uses.

## `graphrt serve`

- **Auth.** Set `GRAPHRT_API_KEY` (or `--api-key-env VAR`). Every `/v1/*` route
  then needs `Authorization: Bearer <key>`, compared in constant time. Only
  `/healthz` is public.
- **No key on loopback.** Without a key on a loopback address, only loopback
  `Host` names are accepted, which blocks DNS-rebinding pages.
- **No key on another address.** Without a key on a non-loopback address, the
  API is open to anyone who can reach it. graphrt prints a warning. The Docker
  image binds `0.0.0.0`, so always set a key there.
- **End-user JWTs.** With `--verify-jwt`, a request whose mapped header holds a
  valid JWT (JWKS or HS256 signature, `exp`, optional `iss` / `aud`, a `sub`)
  may start runs without the API key; an invalid token is `401` even next to a
  valid key. The token opens only `POST /v1/workflows/{name}/runs` of workflows
  that declare the verified input (others are `403`); the server
  refuses to start on a non-loopback address without an API key, so the rest
  of the API stays closed. `--user-rate-limit` caps runs per `sub`. See
  [serving.md](serving.md#verifying-end-user-jwts).
- **CORS.** None. Browsers get no `Access-Control-Allow-Origin`, and JSON
  bodies force a preflight.
- **Limits.**
  - Request bodies: 2 MiB (`--max-body-bytes`).
  - Knowledge uploads: 16 MiB (`--max-upload-bytes`), one ingest at a time.
  - Executing runs: at most 256 at once (`--max-active-runs`); further starts
    and resumes get `429`.
  - Stored runs: `--max-runs` keeps 1,000 in memory; with `--store`, use
    `--store-ttl-days`.
  - Live event feeds and per-run event logs are bounded.
  - Knowledge `top_k` is capped at 100.
- **Identifiers.** Run ids and dataset ids are validated (`[A-Za-z0-9._-]`, no
  leading dot) before any path is built. Workflow names and document ids are
  only lookup keys.
- **Resume with a client snapshot.** `POST /v1/runs/{id}/resume` accepts a
  client-supplied snapshot. Snapshots are not signed, so a caller holding the
  API key can forge node state, for example to skip a human-approval step. Every
  key holder is equally trusted.

## Plugins, WebAssembly and bundles

- **WebAssembly plugins** run in wasmi with:
  - fuel metering (host calls are charged too);
  - a linear-memory cap and small table limits;
  - a recursion limit;
  - a fresh instance per call;
  - only the `graphrt::log`, `chunk` and `progress` imports (no WASI, files,
    network or clock).

  Secrets are masked unless the plugin is trusted.
- **Process plugins** are native code running as your user:
  - they start with an empty environment plus the manifest's `permissions.env`
    (or the full environment with `inherit_env`);
  - optional checksums are verified before every start;
  - each call has a timeout and a message-size cap (at most 256 MiB);
  - the whole process group is killed on drop.

  Treat installing one like installing any program: the manifest's own
  requests are honoured, and only plaintext secrets and bearer tokens need
  `--trust-plugin`.
- **Bundles.**
  - **Signatures:** the Ed25519 signature (strict verification) covers the
    manifest, which pins the SHA-256 of every file. Extra, missing or changed
    files fail. Set `GRAPHRT_TRUSTED_KEYS` (or `--trust-key`) to **require**
    a signature from your keys. Without it, an unsigned bundle is accepted
    and is exactly as trusted as a workflow file plus a providers file.
  - **Extraction:** archives are checked for path traversal, absolute paths,
    symlinks, duplicates, entry count, size and compression ratio. Configs are
    extracted into a fresh owner-only directory.

## Hardening checklist for deployers

| Knob | Where | Default |
| --- | --- | --- |
| API key | `GRAPHRT_API_KEY` / `--api-key-env` | none (warns) |
| Bind address | `--addr` | `127.0.0.1:8080` (Docker: `0.0.0.0:8080`) |
| Private-network egress | `--allow-private-network`, `EgressPolicy::allow_private` | blocked |
| Host allow/deny lists, schemes, redirects, response size, timeouts, proxy | `EgressPolicy` (embedders) | open allow-list, http/https, 5 redirects |
| Code limits ceiling | register a `CodeLimits` service (embedders) | 10 s / 64 MiB / 1 MiB stack / 4 MiB result |
| Python subprocess | `--allow-python-subprocess` | off |
| Concurrent runs | `--max-active-runs` | 256 |
| End-user JWTs, per-user rate limit | `--verify-jwt`, `--jwt-*`, `--user-rate-limit` | off |
| Kept runs | `--max-runs`, `--store-ttl-days` | 1,000 in memory / forever on disk |
| Body and upload size | `--max-body-bytes`, `--max-upload-bytes` | 2 MiB / 16 MiB |
| Run budget | `--timeout-ms`, `--max-steps`, `Limits` | engine defaults |
| Plugin trust | `--trust-plugin`, `GRAPHRT_TRUSTED_PLUGINS`, manifest `checksums` | untrusted |
| Bundle signatures | `--trust-key`, `GRAPHRT_TRUSTED_KEYS` | unsigned accepted |
| OTel content capture | `OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT` (or `GRAPHRT_OTEL_CAPTURE_CONTENT=true`) | off |
| Run inputs in run records | `graphrt serve --store-inputs` | off |

Also:

- Run behind TLS.
- Keep provider keys in the environment or a credential source, never in
  workflow files.
- Declare every key `type: secret`.
- Run the process as an unprivileged user with no access to secrets it does
  not need. Workflows can read any process environment variable they declare
  under `env`.
