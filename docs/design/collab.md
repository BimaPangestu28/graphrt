# Design: real time collaboration

Status: implemented in `graphrt-collab` and `graphrt collab` (cargo feature `collab`).
Guide for operators and users: [docs/collab.md](../collab.md).

## Goal

Graphrt Desktop is single player today: one person builds agents on one
laptop. Teams want to build the same agent together, see who is working on
it, and keep working when the network drops. Graphrt runs a hosted service
for this, and a company can run the same server on its own machines.

Rules this design follows:

- **Modular.** A separate crate (`graphrt-collab`) and a cargo feature
  (`collab`) that is off in every runtime build. A single player user gets
  no new code, no account and no network call.
- **Fast and light.** No database server, no message broker, no JavaScript
  runtime. One small static binary, idle memory in single digit MiB.
- **Secure.** TLS, short lived tokens scoped to a workspace, roles checked on
  every request and every sync message, input checked before anyone else sees
  it, rate limits, an audit log per workspace.
- **Easy for business users.** In Desktop: one address field, Share, Invite,
  avatars of who is there, edits that appear live. No new heavy menus.

## Options considered

| Approach | Merge of concurrent and offline edits | Cost | Verdict |
| --- | --- | --- | --- |
| Lock a project while one person edits | none (others wait) | tiny | not real time, bad offline story |
| Last write wins on the whole document | loses edits | tiny | loses work |
| Operational transform (central server orders operations) | good online, weak offline | server must transform every op, complex to get right | offline merge is the hard part, OT is weakest there |
| CRDT, Automerge | good | larger crate and documents, own sync protocol | heavier than needed |
| **CRDT, Yjs model (`yrs` crate)** | good, proven at scale (Yjs) | small pure Rust crate, compact binary updates, standard sync and presence protocol | **chosen** |

Why Yjs (`yrs`):

- The merge is conflict free by construction: every replica that saw the same
  updates holds the same document, whatever order they arrived in. That is
  exactly what offline editing needs.
- The wire protocol (`y-protocols`: sync step 1 and 2, updates, awareness) is
  a de facto standard. A web client can join with the JavaScript Yjs library
  without new code on the server.
- Shared text (`Y.Text`) merges two people typing in the same prompt
  character by character, which a JSON patch scheme cannot.
- `yrs` is pure Rust with few dependencies, so it builds for every target
  Graphrt supports (Desktop on macOS, Windows, Linux, the musl server image).

## Architecture

```
Graphrt Desktop (A)                        Graphrt Desktop (B)
  local agent (SQLite) <-> ProjectDoc        ProjectDoc <-> local agent
          |  JSON reconcile                        |
          +---- WebSocket (TLS) ----+  +-----------+
                                    v  v
                       graphrt collab serve
       +------------------------------------------------------+
       | HTTP API: sign in, workspaces, members, invites,     |
       |           projects, versions, secrets, audit,        |
       |           services                                   |
       | Rooms: one live ProjectDoc per open project,         |
       |        presence, validation, fan out                 |
       | Files: state.bin + updates.log + versions/ per       |
       |        project, workspace.json, accounts/, audit/    |
       | Plugin host: services of installed plugins           |
       +------------------------------------------------------+
         reuses graphrt-console: roles, audit log, secrets, OIDC
```

### Crate layout

`crates/graphrt-collab`, features:

- default (always): `doc` (the project document and the JSON reconcile),
  `protocol` (sync messages). Pure, no I/O, a few hundred KiB.
- `server`: the collaboration server (axum, tokio, the console crate).
- `services`: plugin services (the graphrt-plugin host).
- `client`: what an app needs (reqwest, a WebSocket client, the loopback sign in).

`graphrt-cli` feature `collab` adds `graphrt collab serve|invite|check|key`.
It is in no default feature set. The container image `:<version>-collab` is
built with only this feature, so it carries no workflow engine.

### The project document

A project is a JSON object of sections. Desktop writes `name`,
`description`, `kind`, `flow` (the workflow document), `layout` (node
positions) and `notes`. Other clients may add sections.

Apps never write CRDT operations by hand. They hand the new JSON to
`ProjectDoc::apply_json`, which compares it with the document and writes the
smallest change:

- objects become maps (different keys edited at once both stay, the same key
  edited twice keeps one value, the same on every replica)
- lists of objects that all carry a distinct `id` (else `name`), such as
  workflow nodes, edges with ids, `env` declarations, are matched item by
  item, so adding, removing and editing different nodes at once all merge
- other lists are matched by value (longest common subsequence)
- prose (keys such as `prompt`, `system`, `instructions`, `description`,
  `template`, or any text with a line break) becomes shared text and merges
  character by character
- everything else is written whole

This keeps the Desktop code unchanged: it keeps saving agents as it does
today, and the collaboration module turns each save into a minimal update.
Text offsets are UTF-16 code units, as Yjs counts them, so web editors
interoperate.

### Sync session

`GET /v1/workspaces/{ws}/projects/{p}/sync`, a WebSocket with subprotocol
`graphrt-collab.v1`. Binary messages are the Yjs sync protocol plus one
custom message (tag 100) holding JSON control messages:

1. The client connects with a workspace scoped access token
   (`Authorization: Bearer`, or the `bearer.<token>` subprotocol for browsers).
2. The server sends `welcome` (who you are, `read_only`), sync step 1 and
   the presence of everyone there.
3. The client answers with sync step 2 (its offline edits) and sends its own
   sync step 1. The server answers with what the client lacks.
4. Then updates and presence flow both ways. Before the token expires the
   client sends a `token` control message with a new one. A role change
   arrives as a `role` message and applies at once. Removal from the
   workspace ends the session (`closing`).

Presence (Yjs awareness) carries what each person looks at (`focus`). The
server overwrites the `user` part of every presence entry with the identity
it verified, binds each replica id to the connection that first used it, and
caps the size, so nobody can appear as someone else.

### Server side checks of every update

Before an update reaches anyone else:

1. The WebSocket message size limit (`limits.max_message_bytes`, 1 MiB).
2. The session's message rate (token bucket, `limits.messages_per_second`).
3. The member's current role: a viewer's real edits are refused (`refused`),
   their handshake replies that bring nothing new are ignored.
4. The update must decode and apply. `yrs` may panic on a crafted update: the
   apply runs under `catch_unwind`, and on any failure the room reloads its
   document from disk and refuses the update.
5. The project's size limit (`limits.max_doc_bytes`, 8 MiB).

Only then is it appended to the log and sent to the others. Twenty refused
messages close the session.

### Storage

Plain files under `data_dir`, written through a temporary file and a rename,
owner only (0600 on Unix):

```
signing.key                      token signing key (when not given by env)
accounts/<acct>.json             account + hashed refresh tokens
audit/                           server audit log (sign ins, workspaces)
workspaces/<ws>/workspace.json   members, invites (hashed), projects, service grants
workspaces/<ws>/audit/           workspace audit log (console format, hash chained)
workspaces/<ws>/secrets.json     workspace secrets (console format, AES-256-GCM)
workspaces/<ws>/projects/<p>/state.bin       compacted document
workspaces/<ws>/projects/<p>/updates.log     length prefixed updates since
workspaces/<ws>/projects/<p>/versions/       index.json + <n>.bin
```

A room (open project) loads `state.bin` plus the log, appends each accepted
update, compacts at 2 MiB or 2000 updates and when it closes (30 seconds
after the last person leaves). A torn last log entry after a crash is skipped.
Applying an update twice is harmless in a CRDT, so a crash between compaction
and log truncation loses nothing.

No database is needed up to many thousands of workspaces on one server. The
file layout is per workspace, so a later move to sharding (workspace affinity
at the load balancer) or to an object store does not change the model.

### Versions and history

- Automatic versions every `limits.version_every_minutes` (10) while people
  edit, and when a room closes with changes. The oldest automatic versions are
  pruned beyond `limits.auto_versions` (100).
- Named versions ("Save version" in Desktop, `POST .../versions`).
- Restore is an edit like any other: the server reconciles the old JSON into
  the live document, so people editing at that moment merge with it. The
  state before the restore is saved as a version first, so a restore can be
  undone.
- Each version lists who edited since the previous one.

## Identity, tokens and roles

### Reuse of the console (graphrt-console)

The admin console (`graphrt serve --console`) already has roles, a hash
chained audit log, encrypted secrets and OpenID Connect. The collaboration
server reuses that crate instead of forking it:

| From the console | Used for |
| --- | --- |
| `Role` (viewer, operator, owner) | workspace roles (operator is shown as "can edit") |
| `audit::AuditLog` | one log per workspace and one for the server |
| `secrets::Secrets` | workspace secrets for plugin services |
| `oidc::Provider` | sign in with the company's (or Graphrt's) identity provider |
| `roles::mapped_role`, `RolesConfig` | who may sign in on a self hosted server |
| `config::OidcConfig`, `AuditConfig`, `SecretsConfig` | the same config blocks |

### Sign in

- **Identity provider (OIDC).** Desktop signs in like a native app (RFC 8252):
  it listens on `127.0.0.1:<random port>`, opens the browser at
  `/v1/auth/login` with its own PKCE challenge, the server runs the provider's
  authorization code flow with PKCE, nonce and state (the console's code),
  and sends the browser back to the loopback address with a one time code
  (60 seconds). Desktop trades it, with its PKCE verifier, for tokens. Only
  `http` loopback redirect addresses are accepted.
- **Invite codes.** For teams without an identity provider: an invite code
  alone makes an account (on by default without OIDC). The first owner of a
  new server gets an owner invite from `graphrt collab invite`.

### Tokens

- **Refresh token** `gcr1.<account>.<secret>`: 256 random bits, kept hashed,
  lasts `auth.refresh_token_days` (30) without use, **rotates on every use**.
  A retired token used again means it was copied: the whole chain (that sign
  in on that device) ends. At most 20 sign ins per account.
- **Access token** `gca1.<payload>.<hmac>`: HMAC-SHA256, lasts
  `auth.access_token_minutes` (10). Account wide (list and create workspaces)
  or **scoped to one workspace** (`POST /v1/auth/scoped`). A sync session
  needs a scoped token for its workspace, and a scoped token is refused on
  every other workspace.
- Tokens never carry permissions. Every request and every sync message reads
  the member's current role, so removing someone or changing a role applies
  at once, to open sessions too.

### Roles in a workspace

| Role | May |
| --- | --- |
| viewer | open projects read only, see who is there, read versions, call viewer services |
| operator (can edit) | edit, create and rename projects, save and restore versions, call services |
| owner | members, roles, invites, services, secrets, the audit log, delete projects and the workspace |

A non member gets 404 for everything of a workspace (outsiders learn
nothing). A member without the role gets 403, written to the audit log as
`denied`. A workspace keeps at least one owner.

### Invites

`gci1.<workspace>.<secret>`, kept hashed, with a role, an expiry (1 hour to
30 days, 7 by default), a use count (owner invites are single use) and
optionally an email that must match the verified email of whoever redeems
it. Invite links are `<public url>/join#<code>`: the code is in the URL
fragment, which browsers never send to a server or a proxy log.

## Hosted and self hosted from one binary

`mode: hosted` | `self_hosted` in the config, same binary and image:

| | hosted (Graphrt) | self hosted (a company) |
| --- | --- | --- |
| Tenancy | many teams, each workspace isolated | one organization |
| Sign up | anyone the identity provider signs in (optionally narrowed by `auth.roles`) | only people `auth.roles` lets in (domains, groups, owners) |
| Workspaces | any account, up to `limits.workspaces_per_account` | people the roles make operator or owner, and workspace owners |
| Identity provider | Graphrt's (owner decision) | the company's (Entra ID, Google Workspace, Okta, Keycloak) or invite codes only |
| Quotas | per account and per workspace (`limits`) | defaults suit a team |

Isolation between tenants: every route resolves the workspace from the
path, checks membership against the account in the token, and only then
touches files under that workspace's directory. Ids are checked against
their format before they become a path. Scoped tokens cannot cross
workspaces.

## Plugin services

An extension point for server side work, built on the existing plugin model
rather than a new one:

```yaml
# graphrt-plugin.yaml of a plugin
runtime: process
command: node
args: [index.js]
permissions: { network: true, secrets: true }
tools:
  - name: deliver
    description: Send a workspace event to the team's webhook
    parameters: { type: object }
services:
  - id: webhook
    title: Send changes to a webhook
    tool: deliver
    access: owner                 # lowest role that may call it directly
    events: [project.changed, version.saved, member.joined]
    secrets: [WEBHOOK_URL, WEBHOOK_SECRET]
    reads_projects: false         # whether project content goes along
```

- `services` is a manifest field of graphrt-plugin, checked like the rest of
  the manifest: the tool must exist, events must be known, secrets need
  `permissions.secrets`. Every host parses it (so `graphrt plugin install`
  and Desktop accept such plugins), only the collaboration server acts on it.
- The server loads plugins from `plugins.dirs` with the same plugin host the
  runtime uses, so the sandbox is the same: a child process with a scrubbed
  environment (or an HTTP service), per request timeout, concurrency and
  message limits, no ambient secrets.
- A service is **off until an owner enables it** for a workspace, granting
  each workspace secret it asks for. Secrets go only to plugins the operator
  lists in `plugins.trusted`, as the tool call's secrets, never through the
  environment.
- Events: `project.changed` (after 3 quiet seconds, so a burst of typing is
  one event), `version.saved`, `member.joined`. Calls:
  `POST .../services/<plugin>/<service>/call` by members with the `access`
  role. Every grant, event delivery and call goes to the audit log.

The first real service is `collab-webhooks` in Graphrt/graphrt-plugins: it
posts workspace events to a webhook URL (Slack, Teams, any HTTP endpoint)
with an HMAC signature. A shared knowledge index and shared tool credentials
fit the same mold (a service that reads projects, a service granted secrets).

## Security summary

| Threat | Answer |
| --- | --- |
| Eavesdropping | TLS: `tls:` in the config (rustls) or a TLS proxy. A non loopback `http://` public URL is refused at startup, Desktop refuses plain http except localhost |
| Stolen access token | lives 10 minutes, scoped to one workspace |
| Stolen refresh token | rotation with reuse detection ends the chain, sign out ends it, disabling an account ends all |
| Outsider guessing ids | random 96 bit ids, 404 for non members, scoped tokens |
| Viewer editing through the socket | role checked per message, real edits refused and counted |
| Impersonation in presence | `user` written by the server, replica ids bound to connections |
| Malformed or huge updates | size limit, decode under `catch_unwind`, reload on failure, document size limit |
| Flooding | per session message rate, per account and per address request rate, sign in attempt rate, session count per account |
| Cross site WebSocket | no cookies (bearer tokens only), `Origin` must be the public URL or `allowed_origins` |
| Invite leaks | hashed at rest, expiry, use count, email binding, code in the URL fragment |
| Plugin abuse | off by default, owner grants per workspace, secrets only for trusted plugins, plugin sandbox and limits |
| Tampering with history | audit log hash chained (console), append only |

## Desktop

Small surface, as the owner asked:

- **Settings > Team server**: one address field (default: Graphrt's hosted
  server, configurable at build time), a "Check connection" button with a
  plain status (connected, wrong address, invalid TLS certificate, sign in
  needed), then Sign in. Saved per profile. Changing servers never deletes
  local projects.
- **Share** button on an agent: choose or create a team, the agent becomes a
  shared project, invite people with a link and a role (can view, can edit).
- **Join a team**: paste an invite link.
- **Avatars** of who is in the project, in the agent's header.
- **Live edits**: changes of others appear in the canvas and the prompt
  boxes as they happen.
- Single player stays exactly as it is: no account, offline, no network call
  until someone chooses Share or Join.

Desktop keeps its SQLite agents as they are. A shared agent remembers its
server, workspace and project, and the collaboration module turns each local
save into an update (`apply_json`) and each remote change into a local save,
so every existing screen keeps working. The local replica is stored, so edits
made offline are sent on the next connection.

## Limits and what comes later

- One process holds a project's room. Scaling out means routing a workspace
  to one instance (workspace affinity). The file layout per workspace allows
  that without a model change.
- No end to end encryption of project content: the server must read updates
  to check them, run services and keep versions. TLS protects transit, the
  data directory belongs on an encrypted volume.
- SAML and SCIM come through the identity provider (as for the console).
