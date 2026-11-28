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

## Parsing untrusted input

- **Workflow documents.** JSON nests at most 128 levels (serde_json). YAML
  runs under serde-saphyr's default budget: 64 levels of nesting, 250,000
  nodes, 64 MiB of scalar text including alias replays, and an
  alias-to-anchor ratio check, so "billion laughs" documents are refused.
- **Templates.** `template-transform` (Jinja) has a fuel limit, a recursion
  limit of 64 and stops rendering at 1 MiB of output. Fuel counts
  instructions, not bytes, and minijinja does not cap intermediate strings:
  `{% set s = s ~ s %}` doubles a string in a handful of instructions, so a
  hostile template can still exhaust memory. Treat `template-transform` in
  untrusted workflows as a known gap. `{{#…#}}` placeholders parse in
  linear time.
- **Variables.** An `assigner` refuses an input variable that holds more than
  1,000,000 values or nests deeper than 128 levels, so a loop that feeds a
  variable into itself cannot double it every round.
- **Documents.** Zip containers are read with entry-count and
  decompressed-size limits. PDF streams are decoded with every filter the
  parser supports (Flate, LZW, ASCII85, chains, PNG predictor rows) against
  `max_decompressed_bytes` (256 MiB) before the document is loaded and again
  before text is extracted. Encrypted object streams are inflated by the
  parser while it loads, before graphrt can measure them. Parser panics are
  caught; CPU time is not bounded, so run extraction under a timeout.

## Agent file workspaces

`--workspace DIR` (crate `graphrt-files`) gives agents the `files` tools.
They are off without the flag.

- **One directory.** Paths are relative and checked before storage is
  touched: no `..`, no absolute paths, no backslashes or drive letters, no
  hidden names, no Windows device names, no control or invisible characters.
- **No links.** A symbolic link or junction anywhere on a path is refused for
  reads, writes, deletes and listings. Only regular files are opened
  (`O_NOFOLLOW`), never FIFOs or devices.
- **Bounded.** 32 MiB per file, 512 MiB and 5,000 files per workspace, and
  per-call caps on what `read` and `search` return.
- **Types from content.** The media type is sniffed from the first bytes. A
  program or a `#!` script is not stored whatever it is called. HTML and SVG
  are stored but flagged `inline_safe: false`: do not render them in a
  webview, and serve them with `Content-Disposition: attachment`.
- **Per user.** With `--workspace-per-user` every call needs a scope from the
  agent's `tool_context`, which the model cannot see or change. Each scope is
  its own directory and quota.
- **Over HTTP only when asked.** `serve --workspace-api` adds `/v1/files`.
  Paths from clients are checked like paths from a model, a verified end
  user reaches only the directory of their own `sub`, uploads stop at
  `--max-upload-bytes`, and every download is sent with `nosniff` and a
  sandboxing `Content-Security-Policy`. HTML, SVG and programs are always
  `application/octet-stream` attachments. The server refuses to start with
  the routes on an address other hosts can reach unless an API key is set.
- **Object storage is opt in.** `--workspace s3://bucket/prefix` (feature
  `files-s3`) keeps the same rules over S3. Requests are signed, redirects
  are not followed, `http://` endpoints must be allowed explicitly, and no
  path reaches outside the prefix. Scope the bucket policy to that prefix.
- **File content is data.** `read` frames content between boundaries that
  change on every call. That lowers the odds of a prompt injection, it does
  not remove them: put destructive and outbound tools under
  `require_approval`.

## Images

With the `image` feature (crate `graphrt-image`, in the default CLI) every
image a model node sends through `vision`, every image the `image` tools
read, and every image a generator returns passes the same checks:

- **Type.** PNG, JPEG, GIF or WebP by magic number. SVG, HTML and HEIC are
  refused whatever MIME type they came with.
- **Size.** At most 20 MiB, 12,000 pixels per side and 25 megapixels, read
  from the header. An image bomb is refused before any decoder runs. Decoders
  run with hard limits, behind a panic boundary, on the blocking pool.
- **Metadata.** EXIF (GPS position, device, time), XMP, IPTC, text chunks,
  comments and anything appended after the image are removed before an image
  leaves the runtime. Files the tools write carry none.
- **Remote images.** `https` image URLs are fetched by the model provider,
  not by graphrt. A generator answer that holds a URL is refused, not
  fetched.
- **Refusals are final.** An image that fails a check fails the node. It is
  never sent as it was.

Text inside an image can try to steer a model, like text inside a file. The
same advice holds: `require_approval` for tools that destroy or publish.

The threat model, the tests behind each line and the residual risks (races on
intermediate directories for a local attacker, hard links) are in
[files-and-images-design.md](files-and-images-design.md#threat-model).

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
  replaces the API key for run starts of workflows that declare the verified
  input, and for the user's own runs (feedback, reading, following, stopping)
  and conversations. The server
  refuses `--verify-jwt` without an API key, the responses are filtered
  (`--user-events`), and `--user-rate-limit` caps requests per `sub`. The
  rules are in
  [serving.md](serving.md#what-a-user-token-may-do).
- **CORS.** None unless `--cors-origin` lists origins: then only those get
  `Access-Control-Allow-Origin` (matched exactly, echoed, with `Vary: Origin`),
  preflights are answered before authentication, and
  `Access-Control-Allow-Credentials` is never sent, so cookies and other
  ambient credentials of a browser are never accepted from another origin.
  `*` needs an API key. Tokens travel in headers only, which is why the API
  needs no CSRF token. See [web.md](web.md#cross-origin-requests-cors).
- **A user's own runs.** A verified user (or a chat guest) reads, follows and
  stops the runs they started, filtered like their stream. Runs of others
  answer `404`. Resuming needs `--user-resume`, because a pause can be an
  approval that someone else has to give.
- **Chat page and guests.** `--chat` serves a page whose
  `Content-Security-Policy` allows scripts, styles and connections of its own
  origin only and framing by its own origin and the `--cors-origin` list
  (Mermaid of `--chat-mermaid` is a file you name, served from that origin).
  `--chat-access guests` lets anyone start the `--chat` workflows, and only
  those, as a guest with a signed, stateless token, within
  `--chat-guest-rate-limit` and `--chat-guest-budget`. See
  [web.md](web.md#guests).
- **Page context is data, not identity.** `sys.context`, which the chat widget
  sends with every message, is checked for its shape and size only (a JSON
  object, at most 8 KiB and 5 levels, else `400`). A build without the
  `web-chat` feature refuses any context. Nothing in the server
  reads it: the user is the verified token. Treat it in a workflow like the
  visitor's message, never as proof of who they are or what they may do. See
  [web.md](web.md#page-context).
- **Device tokens.** `--devices` pairs a device with a token that lists and
  reads runs and resolves approvals, and nothing else: it starts, stops and
  deletes nothing and pairs no device. Tokens are 256 random bits, stored as
  their SHA-256, shown once, and end when their device is removed. See
  [monitoring.md](monitoring.md#devices-a-token-for-a-phone).
- **Monitoring page.** `--monitor` serves a page whose
  `Content-Security-Policy` allows scripts, workers and connections of its own
  origin only and forbids framing. It holds no data: what it shows comes from
  the API with the key or device token the person entered. A pairing link
  carries the token in the URL fragment, which is never sent to a server.
- **Push notifications.** `--push` sends requests to the push services of
  subscribed browsers, through the egress policy (`https`, no private
  addresses, no redirects). Messages are encrypted for the browser, and the
  server's signing key and the subscriptions stay in the store directory.
  See [monitoring.md](monitoring.md#notifications).
- **Conversations.** `--conversations` keeps chat turns in the run store,
  scoped to the verified user, with limits and retention. They hold what
  people wrote: protect the store directory like run records.
- **UI blocks in a browser.** Blocks come from models and tools. The web
  renderer validates each block before drawing, builds the page without
  markup strings, and loads no code. See [web.md](web.md#drawing-ui-blocks).
- **Limits.**
  - Request bodies: 2 MiB (`--max-body-bytes`).
  - Knowledge uploads: 16 MiB (`--max-upload-bytes`), one ingest at a time.
  - Workspace uploads (`--workspace-api`): the same limit, four held in
    memory at once, dropped after 30 seconds without data.
  - Executing runs: at most 256 at once (`--max-active-runs`); further starts
    and resumes get `429`.
  - Stored runs: `--max-runs` keeps 1,000 in memory; with `--store`, use
    `--store-ttl-days`.
  - Live event feeds and per-run event logs are bounded.
  - Knowledge `top_k` is capped at 100.
- **Files on disk.** The `--store` directory (records, events, snapshots,
  scores), replay cassettes and `graphrt eval` reports are created owner-only
  on Unix (`0600` files, `0700` directories); existing files keep their mode.
- **Identifiers.** Run ids and dataset ids are validated (`[A-Za-z0-9._-]`, no
  leading dot) before any path is built. Workflow names and document ids are
  only lookup keys.
- **Behind a gateway (`--trust-gateway`).** The `/internal/v1` routes accept
  only requests signed with HMAC-SHA256 by a key of `$GRAPHRT_GATEWAY_KEYS`
  (32+ bytes, key ids for rotation) over the method, path and query,
  deployment, run id, epoch and body hash, within ±30 s; each signature is
  accepted once (replays are refused even within the window) and requests for
  another `--deployment` get `403`. The private network between gateway and
  agents is not assumed to be a trust boundary (Fly's private network spans an
  organization, pod networks are flat): add NetworkPolicy or an internal
  ingress on top. Agents never call the gateway (it pulls orphans from them),
  so an agent needs no gateway address and no outbound access. `--trust-gateway` and
  `--verify-jwt` are exclusive. See [gateway-protocol.md](gateway-protocol.md#signatures).
- **Standby agents.** A gateway's pre-started local agents
  (`standby_agents`, see [gateway.md](gateway.md#standby-agents)) start with
  no deployment, no keys and without the gateway's own secret variables
  (deployment keys, admin token, callback and webhook secrets). A standby gets
  one deployment's keys in its assignment over its stdin pipe (never in its
  environment, command line or logs), serves that deployment only and is never
  reused; one whose assignment fails is killed.
- **Unix sockets.** `--listen unix:/path` creates the socket mode `0600`.
- **Named pipes (Windows).** `--listen pipe:\\.\pipe\name` refuses remote
  clients and a name that exists already (first instance only); the default
  pipe DACL lets only the owner, SYSTEM and administrators write. The gateway
  names its agents' pipes with 128 random bits.
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
  - the whole process group is killed on drop;
  - the manifest's `cwd` must stay inside the plugin directory, and a relative
    `command` may not climb out of it with `..`.

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
    symlinks, duplicates, entry count, size and compression ratio. Windows
    device names (`CON`, `nul.txt`, `COM1`...) and names that differ only in
    case are refused before anything is written. Configs are extracted into a
    fresh owner-only directory.

## Skills

A skill is treated as untrusted content that the owner chose to install.

- **No code.** graphrt never executes a skill's files. `scripts/` is text.
- **Tools.** A skill lists the tools it wants. Only grants stored in the skill
  store's index count, and a grant must name a tool the skill asked for. A
  granted tool is offered to the model after the skill was loaded in that run
  and asks for approval before each call unless it was granted with `--allow`.
- **Pinned content.** Every file is recorded with its SHA-256 at install and
  checked on each read. Changed, added or linked files are not served. An
  update is a new install with no grant carried over.
- **Reads stay inside the skill.** File reads are looked up in the index by
  exact path. Nothing a model sends is joined to a directory.
- **Prompt text.** Names and descriptions go into the system prompt, so
  invisible characters and markup tags in them are refused. Instructions reach
  the model as a tool result inside a labelled wrapper that the text cannot
  close.
- **Catalog installs need a signature.** `graphrt skill install <name>` uses
  an index only after its minisign signature verifies against the catalog
  key, checks the archive's sha256 against that index, and then runs the same
  install as for a directory. There is no unsigned mode.
- **A catalog install over HTTP cannot choose its catalog.**
  `POST /v1/skills/catalog/install` reads the index and the key the server
  was started with. The request has no field for an index, a key, an archive
  or an unsigned mode, and unknown fields are refused.
- **A model's draft is only a draft.** `graphrt skill draft` and
  `POST /v1/skills/drafts/model` return text. Nothing is installed and no
  tool is granted, whatever the source or the model says. Installing is a
  separate owner action through the same gate.
- **A served bundle keeps to its own skills.** A bundle that brings skills
  runs on a private store. It cannot read a skill of the server, so it never
  inherits a grant the server's owner gave. The server's routes cannot change
  the bundle's skills either. A bundle's grants are limited to tools its own
  skills ask for, name only tools the bundle's workflow could list itself,
  and are printed at startup. A bundle that fails the install gate is not
  served.
- **Managing skills is an owner action.** `/v1/skills` needs the API key. A
  user token or a device token (`--devices`) is not accepted, and a server
  that listens on a network address without a key refuses these routes.
- **The scan is advisory.** It reports common hostile patterns before an
  install and can be evaded by rewording. The points above do not rely on it.
- **Not covered.** A skill's instructions can still mislead a model within the
  tools the agent already has. Give agents that read untrusted skills only the
  tools they need, and keep approval on for tools that send or change things.

See [skills.md](skills.md).

## Self learning

What an agent learned is treated as untrusted until a person approved it.

- **Runs only read.** An `agent` node never writes a lesson. Signals are
  recorded by the host and turned into proposals outside the run.
- **Only approved lessons are used.** Proposed, rejected and retired lessons
  never reach a prompt.
- **Derived content never approves itself.** A lesson built from tool results,
  documents, model output or feedback that was not authenticated can only be a
  proposal, whatever the policy says.
- **Scope is exact.** A run reads the lessons of its agent and of its own user.
  A lesson that came from one user stays with that user until the owner
  promotes it. Agent names are validated before any path is built.
- **Prompt text.** A lesson is one line of at most 500 characters without
  invisible characters or markup tags, placed in a section that says it does
  not override the instruction or approval rules.
- **Whose words feedback is.** Under `graphrt serve`, feedback counts as a
  user's own words only when it arrives with that user's verified token
  (`--verify-jwt`) on a run that token started. A token that does not verify
  is refused, also next to a valid API key. Another user's token is refused.
  Feedback sent with the API key is `derived` whatever `user` its body names.
- **Behind a gateway.** An agent run with `--trust-gateway` takes the user of
  a run from a request the gateway signed and from nowhere else. The gateway
  sets `user_id` and `sys.user_id` to the user it verified, over anything the
  client sent, and says so with `caller: user`. A run request without
  `caller` (a gateway older than the field) names no user: the agent drops
  its `sys.user_id`, so an old gateway in front of a new agent cannot make a
  run read another user's lessons. Feedback sent to the agent's own route is
  always `derived`. Feedback of an end user goes through the gateway's
  `POST /v1/deployments/{d}/runs/{id}/feedback`: the user's token, on a run
  started with a token of the same user, makes it their own words, with the
  same rules as `--verify-jwt` (another user's token is `403`, a missing or
  bad one `401`, risky lessons wait for the owner). A gateway signature does
  not open `/v1/skills` or `/v1/learning`.
- **A model only proposes.** What a model suggests when distilling
  (`graphrt learn distill --model`, `POST /v1/learning/{agent}/distill`) is a
  proposal with trust `derived`. A skill drafted from lessons installs
  nothing.
- **Managing lessons is an owner action.** `/v1/learning` needs the API key. A
  user token or a device token (`--devices`) is not accepted, and a server
  that listens on a network address without a key refuses these routes.
- **Removal.** Lessons can be retired, deleted, or forgotten by user or by
  run. Every change is appended to an audit log.
- **Not covered.** The owner can approve a harmful lesson. The risk label and
  its reasons are there to help that decision and can be evaded by rewording.
  The host must mask secrets in what it records and must say truthfully
  whether a feedback author was authenticated.

See [learning.md](learning.md).

## Hardening checklist for deployers

| Knob | Where | Default |
| --- | --- | --- |
| API key | `GRAPHRT_API_KEY` / `--api-key-env` | none (warns) |
| Bind address | `--addr` | `127.0.0.1:8080` (Docker: `0.0.0.0:8080`) |
| Private-network egress | `--allow-private-network`, `EgressPolicy::allow_private` | blocked |
| Host allow/deny lists, schemes, redirects, response size, timeouts, proxy | `EgressPolicy` (embedders) | open allow-list, http/https, 5 redirects |
| Code limits ceiling | register a `CodeLimits` service (embedders) | 10 s / 64 MiB / 1 MiB stack / 4 MiB result |
| Python subprocess | `--allow-python-subprocess` | off |
| Agent file workspace | `--workspace`, `--workspace-read-only`, `--workspace-per-user` | off |
| Workspace over HTTP | `--workspace-api key\|users` | off |
| Image tools and generation | `--image-tools`, `--image-model` | off |
| Document tools (PDF, Word, Excel) | `--document-tools` | off |
| HEIC input | cargo feature `image-heic` | not compiled |
| Image limits | `ImageLimits` (embedders) | 20 MiB, 25 megapixels, 1,568 px to models, metadata removed |
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
