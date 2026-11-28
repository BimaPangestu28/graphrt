# Collaboration server

`graphrt collab` lets a team build agents together: everyone in a workspace
opens the same agent in Graphrt Desktop, sees who else is there, and sees
their edits to the graph, the prompts and the layout as they happen. Edits
made offline merge when the laptop is back online. Graphrt runs a hosted
server, and a company can run the same server on its own machines.

Single player use does not change: without a server, Graphrt Desktop works
offline with no account, as before.

How it works inside: [design/collab.md](design/collab.md).

## Contents

- [Quick start (local)](#quick-start-local)
- [Hosted or self hosted](#hosted-or-self-hosted)
- [Config](#config)
- [Sign in](#sign-in)
- [Roles](#roles)
- [Deploy](#deploy): container, compose with HTTPS, Kubernetes (Helm), TLS without a proxy
- [Plugin services](#plugin-services)
- [Security](#security)
- [API](#api)
- [Operations](#operations): backups, logs, limits, upgrades

## Quick start (local)

```sh
cat > collab.yaml <<'EOF'
public_url: http://127.0.0.1:8790
data_dir: ./collab-data
EOF
graphrt collab invite --config collab.yaml --workspace "My team"   # prints a link
graphrt collab serve --config collab.yaml
```

In Graphrt Desktop: **Settings > Team server**, enter `http://127.0.0.1:8790`,
**Check connection**, then **Share > Join a team** and paste the link. The
link works once and makes you the owner of "My team". Share an agent, invite
others from the Share dialog.

Plain `http://` works for `localhost` only. A server other people use needs
HTTPS (see [Deploy](#deploy)).

## Hosted or self hosted

One binary, one image, `mode` in the config:

| | `hosted` | `self_hosted` (default) |
| --- | --- | --- |
| For | Graphrt's service, many teams | one company on its own server |
| Who signs in | anyone the identity provider signs in, narrowed by `auth.roles` when set | only who `auth.roles` lets in (domains, groups, owners) |
| Who creates workspaces | any account, up to `limits.workspaces_per_account` | people the roles make `operator` or `owner`, and owners of a workspace |
| Quotas | per plan (`limits`) | defaults suit a team |

Workspaces are isolated from each other in both modes: separate directories,
membership checked on every request, access tokens scoped to one workspace.

## Config

```yaml
mode: self_hosted                       # or hosted
public_url: https://collab.example.com  # what people enter in Desktop, https:// unless loopback
listen: 0.0.0.0:8790                    # default 127.0.0.1:8790
data_dir: /var/lib/graphrt-collab
tls: { cert: /etc/graphrt/tls.crt, key: /etc/graphrt/tls.key }   # or a TLS proxy in front
behind_proxy: false                     # true: client address from X-Forwarded-For (rate limits)
allowed_origins: []                     # web pages that may open sync sessions, besides public_url

auth:
  oidc:                                 # optional: the company's identity provider
    issuer: https://login.microsoftonline.com/<tenant>/v2.0
    client_id: 00000000-0000-0000-0000-000000000000
    client_secret_env: GRAPHRT_COLLAB_OIDC_SECRET
    groups_claim: groups
  providers: []                         # or several providers, each a sign in button (below)
  roles:                                # the console's role rules
    owners: [it-admin@example.com]
    groups: { graphrt-builders: operator }
    allowed_domains: [example.com]
    default_role: viewer
  invite_accounts: false                # invite codes make accounts (default: on without a provider)
  access_token_minutes: 10
  refresh_token_days: 30
  signing_key_env: GRAPHRT_COLLAB_SIGNING_KEY

limits:
  workspaces_per_account: 5             # hosted
  projects_per_workspace: 200
  members_per_workspace: 100
  max_doc_bytes: 8388608                # one project, encoded
  max_message_bytes: 1048576            # one sync message
  sessions_per_account: 20
  messages_per_second: 60               # per sync session
  requests_per_minute: 300              # per account, or per address signed out
  auto_versions: 100
  version_every_minutes: 10

audit: { retention_days: 365 }
secrets: { key_env: GRAPHRT_COLLAB_SECRETS_KEY }   # or key_file
plugins: { dirs: [/plugins], trusted: [collab-webhooks] }
```

`oidc`, `roles`, `audit` and `secrets` are the blocks of the
[admin console](console.md), with the same meaning. Unknown keys are errors.
Check a config with `graphrt collab check --config collab.yaml`.

Keys, from the environment (never in the config file):

| Variable | What | Without it |
| --- | --- | --- |
| `GRAPHRT_COLLAB_SIGNING_KEY` | signs access tokens (32 bytes, hex or base64) | a key generated in `data_dir/signing.key` |
| `GRAPHRT_COLLAB_SECRETS_KEY` | encrypts workspace secrets (AES-256-GCM) | secrets cannot be set |
| `oidc.client_secret_env` | the identity provider's client secret | a public client (PKCE only) |
| `providers[].client_id_env` | a provider's client id | that provider is off (the server starts without it and says so) |
| `providers[].client_secret_env` | a provider's client secret | the server does not start while the client id is set |

`graphrt collab key` prints a new random key.

## Sign in

**With the identity provider.** Register the server as a web application at
the provider, with the redirect URI `<public_url>/v1/auth/callback`. Desktop
opens the browser, the person signs in at the provider, and the browser goes
back to Desktop on a loopback address (`http://127.0.0.1:<port>/callback`, as
native apps do). Desktop never sees the password.

**With several providers.** `auth.providers` lists them, and Desktop shows
one button each (e.g. "Sign in with Google" and one for the company's own
provider). All of
them use the same redirect URI. An account is the provider's issuer plus the
person's subject, so the same person at two providers has two accounts.

```yaml
auth:
  providers:
    - id: google                        # a to z, 0 to 9, -: what the app sends
      name: Google                      # the button (default: from the issuer)
      issuer: https://accounts.google.com
      client_id_env: GRAPHRT_COLLAB_GOOGLE_CLIENT_ID
      client_secret_env: GRAPHRT_COLLAB_GOOGLE_CLIENT_SECRET
      auth_params: { prompt: select_account }
    - id: acme                          # the company's own provider (Keycloak, Okta, Entra ID, ...)
      name: Acme
      issuer: https://sso.acme.example/realms/staff
      client_id_env: GRAPHRT_COLLAB_ACME_CLIENT_ID
      client_secret_env: GRAPHRT_COLLAB_ACME_CLIENT_SECRET
```

Each entry also takes `client_id` (instead of `client_id_env`), `scopes`
(default `openid email profile`), `groups_claim` and `auth_params`, as `oidc`
does. A provider whose `client_id_env` is empty is off until it is set, so a
server can go live before every provider is registered. `GET /v1/info` lists
the providers that are on, and `GET /v1/auth/login?provider=<id>` picks one
(the first without it).

An issuer is one address: each ID token must name exactly the issuer of the
discovery document (for Entra ID, the issuer of the company's tenant,
`https://login.microsoftonline.com/<tenant>/v2.0`). An email is used unless
the token says `email_verified: false`. For a provider that does not say
`email_verified`, `email_verified_claim: <claim>` keeps an email only when
that claim is `true`. Without a verified email the person still signs in, but
invites sent to an email address and `allowed_domains` do not match them.

**With invite codes only.** Without a provider (or with `invite_accounts: true`),
an invite link alone makes an account with the name the person types. This
fits small teams and guests. The first owner of a new server joins with:

```sh
graphrt collab invite --config collab.yaml --workspace "Acme" [--role owner] [--email a@x.com] [--hours 72]
```

It works next to a running server. The link is `<public_url>/join#<code>`:
the code is after `#`, so it never reaches a server log.

Sessions: a sign in lasts `refresh_token_days` without use. Access tokens last
`access_token_minutes` and are renewed by the app. A refresh token rotates on
every use, and one used twice (copied) ends that sign in.

## Roles

The console's three roles, in each workspace:

| Role | In Desktop | May |
| --- | --- | --- |
| `viewer` | Can view | open shared agents read only, see who is there, see versions |
| `operator` | Can edit | edit, share new agents, save and restore versions |
| `owner` | Owner | invite and remove people, change roles, services, secrets, the audit log, delete |

Role changes and removals apply at once, to open sessions too. A workspace
keeps at least one owner.

## Deploy

The image `ghcr.io/graphrt/graphrt:<version>-collab` (tag `collab` for the
latest) holds only the collaboration server: about 9 MB, about 4 MiB of
memory when idle. Run it with `collab serve --config ...`.

### Compose with automatic HTTPS

`deploy/collab/` has `compose.yaml` (the server and Caddy, which gets a
Let's Encrypt certificate), `Caddyfile` and `collab.yaml`:

```sh
cd deploy/collab
# set the DNS name in Caddyfile and public_url in collab.yaml
echo "GRAPHRT_COLLAB_SIGNING_KEY=$(docker run --rm ghcr.io/graphrt/graphrt:collab collab key)" > .env
echo "GRAPHRT_COLLAB_SECRETS_KEY=$(docker run --rm ghcr.io/graphrt/graphrt:collab collab key)" >> .env
docker compose up -d
docker compose exec collab graphrt collab invite --config /etc/graphrt/collab.yaml --workspace "My team"
```

### Kubernetes (Helm)

```sh
kubectl create secret generic graphrt-collab-keys \
  --from-literal=GRAPHRT_COLLAB_SIGNING_KEY=$(graphrt collab key) \
  --from-literal=GRAPHRT_COLLAB_SECRETS_KEY=$(graphrt collab key)
helm install collab deploy/collab/helm/graphrt-collab \
  --set config.public_url=https://collab.example.com --set ingress.host=collab.example.com
```

One replica with a persistent volume (a project's live room lives in one
process). The ingress keeps WebSockets open for an hour, the app reconnects
on its own after that.

### TLS without a proxy

```yaml
tls: { cert: /etc/graphrt/tls.crt, key: /etc/graphrt/tls.key }
```

PEM files, rustls with TLS 1.2 and 1.3. A certificate from a company CA works
when the CA is trusted by the computers running Desktop (Desktop uses the
system's trust store).

### Hosted service

`deploy/collab/collab-hosted.yaml` is the config of Graphrt's hosted server
at `https://collab.graphrt.com`: `mode: hosted`, open sign up with Google,
invite codes for people without it, and the free plan's quotas:

| Limit | Free plan | Self hosted default |
| --- | --- | --- |
| `workspaces_per_account` | 2 | 5 |
| `projects_per_workspace` | 10 | 200 |
| `members_per_workspace` | 5 | 100 |
| `max_doc_bytes` (one project) | 2 MiB | 8 MiB |
| `max_message_bytes` | 512 KiB | 1 MiB |
| `sessions_per_account` | 5 | 20 |
| `messages_per_second` (per session) | 30 | 60 |
| `requests_per_minute` | 120 | 300 |
| `auto_versions` (per project) | 30 | 100 |

They are config, so a plan change is a config change and a restart. A
person over a quota gets an error that names the limit.

`deploy/collab/hosted/` has the Kubernetes manifests of the hosted server
(k3s, ingress-nginx, cert-manager), its deploy script, daily backups with a
restore script, and a smoke test of the live server.

## Plugin services

Plugins can run server side services for the workspaces that enable them,
for example sending changes to a team chat, a shared knowledge index, or
tool credentials kept as workspace secrets. A plugin declares them in its
manifest (`services`, see [plugin-manifest.md](plugin-manifest.md#services)).

```yaml
plugins:
  dirs: [/plugins]               # installed plugins (graphrt plugin install --dir /plugins ...)
  trusted: [collab-webhooks]     # plugins that may receive the secrets workspaces grant
```

- A service is off until an owner enables it for a workspace:
  `PUT /v1/workspaces/{ws}/services/{plugin}/{service}` with
  `{"enabled": true, "secrets": ["WEBHOOK_URL"]}`. Secrets are set first with
  `PUT /v1/workspaces/{ws}/secrets/{name}` (owners, values never shown again).
- Events: `project.changed` (a few seconds after editing stops),
  `version.saved`, `member.joined`. Project content goes along only to
  services that declare `reads_projects`.
- Calls: `POST /v1/workspaces/{ws}/services/{plugin}/{service}/call` by members
  with the service's `access` role.
- The plugin runs like any graphrt plugin: its own process with a scrubbed
  environment (or an HTTP service), timeouts and size limits. Every grant,
  event and call is in the workspace's audit log.

The `collab-webhooks` plugin of the
[plugin catalog](https://github.com/Graphrt/graphrt-plugins) posts events to
a webhook (Slack, Microsoft Teams, any HTTPS endpoint) with an HMAC signature.

## Security

- **TLS** on the server or a proxy. A non loopback `http://` public URL stops
  the server at startup, Desktop refuses plain http except localhost.
- **Tokens**: access tokens last 10 minutes and are scoped to one workspace
  for sync. Refresh tokens rotate, are kept hashed, and a copied one ends its
  sign in.
- **Authorization** on every request and every sync message: non members get
  404, members without the role 403 (audited as `denied`), viewers' edits are
  refused.
- **Input**: every update is size checked, decoded and applied before anyone
  else sees it. A bad one is refused and the project reloads from disk.
  Presence is rewritten with the identity the server verified.
- **Rate limits** per sync session, per account, per address and on sign in.
- **Audit log** per workspace (the console's, hash chained):
  `GET /v1/workspaces/{ws}/audit` for owners.
- **At rest**: files only the server's user can read. Put `data_dir` on an
  encrypted volume. Workspace secrets are encrypted with the secrets key.

## API

All under `<public_url>`. JSON bodies, `Authorization: Bearer <access token>`
except the first group.

| Route | Who | What |
| --- | --- | --- |
| `GET /healthz`, `GET /readyz` | anyone | probes |
| `GET /v1/info` | anyone | server, version, mode, sign in methods (what Desktop's "Check connection" reads) |
| `GET /join` | anyone | the page an invite link opens |
| `GET /v1/auth/login` | apps | start a sign in (`redirect_uri` loopback, `state`, `code_challenge` S256) |
| `POST /v1/auth/token` | apps | `authorization_code` (code, code_verifier, redirect_uri) or `refresh_token` |
| `POST /v1/auth/logout` | apps | end a sign in (`refresh_token`) |
| `POST /v1/invites/preview` | anyone | what an invite is for |
| `POST /v1/invites/redeem` | anyone | join (signed in, or a new account where allowed) |
| `POST /v1/auth/scoped` | signed in | an access token for one workspace |
| `GET /v1/me` | signed in | the account, its workspaces, its sign ins |
| `GET, POST /v1/workspaces` | signed in | list, create |
| `GET, PATCH, DELETE /v1/workspaces/{ws}` | viewer, owner, owner | details, rename, delete |
| `GET /v1/workspaces/{ws}/members`, `PUT, DELETE .../members/{account}` | viewer, owner | members, role, remove (anyone may leave) |
| `GET, POST /v1/workspaces/{ws}/invites`, `DELETE .../invites/{id}` | owner | invites |
| `GET, POST /v1/workspaces/{ws}/projects` | viewer, operator | list, create (with first `content`) |
| `GET, PATCH, DELETE /v1/workspaces/{ws}/projects/{p}` | viewer, operator, owner | content and who is online, rename, delete |
| `GET .../projects/{p}/sync` | viewer | the sync WebSocket (workspace scoped token) |
| `GET, POST .../projects/{p}/versions`, `GET .../versions/{n}`, `POST .../versions/{n}/restore` | viewer, operator | versions |
| `GET /v1/workspaces/{ws}/audit` | owner | the audit log (filters of the console's audit API) |
| `GET /v1/workspaces/{ws}/secrets`, `PUT, DELETE .../secrets/{name}` | owner | workspace secrets (names only) |
| `GET /v1/workspaces/{ws}/services`, `PUT .../services/{plugin}/{service}`, `POST .../call` | viewer, owner, per service | plugin services |

Errors are `{"error": {"code", "message"}}` with codes `unauthorized`,
`forbidden`, `not_found`, `bad_request`, `conflict`, `rate_limited`, `limit`.

The sync protocol is the Yjs sync protocol (y-protocols) over a binary
WebSocket with subprotocol `graphrt-collab.v1`, plus Graphrt control messages
(Yjs custom message 100). Browsers pass the token as the subprotocol
`bearer.<token>`. A Rust app uses the `client` feature of `graphrt-collab`
(`Api`, `LiveProject`, `LoopbackLogin`).

## Operations

- **Backups**: copy `data_dir` (plain files). A copy taken while the server
  runs is consistent per project: `state.bin` is replaced atomically and the
  log is append only.
- **Logs**: stderr. Sign ins and workspace changes are in `data_dir/audit/`,
  everything of a workspace in `workspaces/<id>/audit/`.
- **Limits**: see [Config](#config). A full project refuses edits with a
  message in Desktop. A slow connection is dropped and resyncs.
- **Upgrades**: stop, replace the binary or image, start. Open sessions are
  told to leave, the apps reconnect and merge.
- **Scale**: one process serves many workspaces. Running several processes
  needs routing by workspace (one workspace on one process).
