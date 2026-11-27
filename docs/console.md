# Admin console

For teams and companies that run `graphrt serve` on their own servers (on
premises, in a VPC, air gapped). `graphrt serve --console console.yaml` adds:

- **Sign in with OpenID Connect**: Entra ID, Google Workspace, Okta, Keycloak
  or any other provider, for the people who operate the server. The API key
  keeps working for automation.
- **Roles**: owner, operator and viewer, mapped from the groups in the ID token
  or set by an owner, checked on every route of the server.
- **An audit log**: who did what and when, append only and hash chained,
  queryable, exported as JSON lines, with retention.
- **Secrets**: the environment variables of the served workflows, set from the
  console, encrypted at rest, never shown again.
- **An agent registry**: every version of each workflow by content hash,
  environments such as `dev` and `prod`, promotion and rollback.

It is in the `graphrt-console` crate, built into `graphrt` with the cargo
feature `console` (`cargo install graphrt-cli --features console`), and off
unless `--console` (or `$GRAPHRT_CONSOLE`) names a config. Without it nothing
changes. SAML, SCIM and log streaming are not part of it.

- [Quick start](#quick-start)
- [The config](#the-config)
- [Signing in](#signing-in)
- [Roles](#roles)
- [Audit log](#audit-log)
- [Secrets](#secrets)
- [Agent registry](#agent-registry)
- [Applying changes](#applying-changes)
- [API](#api)
- [Security notes](#security-notes)

## Quick start

1. Register an application at your identity provider (a "web" application with
   the authorization code flow). The redirect URI is
   `<public_url>/console/callback`, e.g.
   `https://agents.example.com/console/callback`.
2. Write `console.yaml`:

   ```yaml
   public_url: https://agents.example.com
   environments: [dev, prod]
   environment: prod
   oidc:
     issuer: https://login.microsoftonline.com/<tenant id>/v2.0
     client_id: <application id>
     client_secret_env: GRAPHRT_OIDC_CLIENT_SECRET
     groups_claim: groups
   roles:
     owners: [alice@example.com]
     groups:
       <group id of the platform team>: owner
       <group id of the operators>: operator
       <group id of everyone else>: viewer
   ```

3. Start the server with an API key, a store and a key for the secrets:

   ```sh
   export GRAPHRT_API_KEY=...                       # automation keeps using it
   export GRAPHRT_OIDC_CLIENT_SECRET=...
   export GRAPHRT_CONSOLE_KEY="$(openssl rand -base64 32)"   # keep it: it decrypts the secrets
   graphrt serve flows/ --addr 0.0.0.0:8080 --store /var/lib/graphrt --console console.yaml
   ```

4. Open `https://agents.example.com/console/login`.

The console needs the API key: `graphrt serve` refuses `--console` without one.
Serve it over HTTPS (a reverse proxy or load balancer in front is fine): the
session cookie is `Secure` and `__Host-` prefixed then. Plain HTTP is accepted
for `localhost` and `127.0.0.1` only, for trying it out.

## The config

| Key | Default | What |
| --- | --- | --- |
| `public_url` | required | The address people open. Its host is the only `Host` a signed in browser may send. A path is a reverse proxy prefix. |
| `allowed_hosts` | `[]` | More `Host` values for signed in browsers (`name` or `name:port`). |
| `data_dir` | `<--store>/console` | Where the console keeps its files (below). Several servers may share it. |
| `environments` | `[dev, prod]` | The environments, in the order versions are promoted. |
| `environment` | the last one | The environment this server serves. |
| `oidc.issuer` | required | The provider. `<issuer>/.well-known/openid-configuration` must describe it. `https` (`http` for localhost only). |
| `oidc.client_id` | required | The application id at the provider. |
| `oidc.client_secret_env` | none | The variable with the client secret. Without one the console signs in as a public client (PKCE only). |
| `oidc.scopes` | `[openid, email, profile]` | Scopes asked for. Add `groups` for Okta and Keycloak when they need it. |
| `oidc.groups_claim` | `groups` | The ID token claim that lists the person's groups. Entra ID app roles are in `roles`. |
| `oidc.auth_params` | `{}` | More parameters of the authorization request, e.g. `{hd: example.com}` (Google Workspace) or `{prompt: select_account}`. |
| `roles.owners` | `[]` | Always owners, by verified email or by subject. |
| `roles.groups` | `{}` | Group -> role. The highest role that applies wins. |
| `roles.default_role` | none | The role of a person no rule names. Without one, they cannot sign in. |
| `roles.allowed_domains` | any | Email domains that may sign in (verified emails only). |
| `sessions.idle_minutes` | `60` | A session ends after this long without a request. |
| `sessions.max_hours` | `12` | A session ends after this long, whatever happens. |
| `audit.retention_days` | `365` | Days an audit entry is kept. |
| `secrets.key_env` | `GRAPHRT_CONSOLE_KEY` | The variable with the key of the secrets: 32 bytes as base64 or hex. |
| `secrets.key_file` | none | A file with the key (a mounted secret), read when the variable is not set. |

The data directory holds `members.json` (who signed in, the roles owners set),
`sessions.json` (sessions, by the SHA-256 of their tokens), `secrets.json`
(sealed values), `registry/` (versions and environments) and `audit/` (one
JSON lines file per UTC day). Files are created owner only and replaced
atomically. Back it up with the run store.

### Providers

| Provider | `issuer` | Groups |
| --- | --- | --- |
| Entra ID | `https://login.microsoftonline.com/<tenant id>/v2.0` | Turn on "groups claim" in the app registration (`groups` holds group object ids), or define app roles and set `groups_claim: roles`. |
| Google Workspace | `https://accounts.google.com` | Google puts no groups in ID tokens: use `roles.owners`, `default_role` with `allowed_domains`, and roles set in the console. `auth_params: {hd: example.com}` preselects the domain. |
| Okta | `https://<org>.okta.com` or a custom authorization server | Add a `groups` claim to the ID token in the authorization server. |
| Keycloak | `https://<host>/realms/<realm>` | Add the "Group Membership" mapper to the client (claim `groups`, full path off). |

## Signing in

`/console/login` sends the browser to the provider with the authorization code
flow, PKCE (S256), a random `state` and a random `nonce`. The `state` is bound
to the browser that started the sign in by a short lived cookie, so a sign in
cannot be completed in another browser. At `/console/callback` the console
exchanges the code (with the client secret, at the token endpoint only),
checks the ID token's signature against the provider's keys (RS256 or ES256,
fetched again when the provider rotates them), its issuer, audience, `azp`,
expiry and nonce, maps the person's role and starts a session.

A session is a random 256 bit token in a cookie that is `HttpOnly`,
`SameSite=Strict`, and over HTTPS `Secure` with the `__Host-` prefix. The
server keeps its hash only. Each session has a CSRF token
(`GET /v1/console/session` gives it to the page) that every request that
changes something must send in `X-Graphrt-CSRF`, from the console's own origin.
A signed in request must name an allowed `Host`, which keeps DNS rebinding
out. Requests with a bearer token (the API key, a device token, an end user
token) are not affected by any of this and keep their own rules.

Sessions survive restarts and end after `sessions.idle_minutes` without a
request or `sessions.max_hours` in all. Signing out (`POST
/v1/console/sign-out`) ends the session, and its answer names the provider's
`end_session_endpoint` when it has one.

## Roles

| Role | May |
| --- | --- |
| viewer | read: workflows, runs and their events, the approvals inbox, the registry, skills, lessons, the knowledge base |
| operator | what a viewer may, and start, stop, resume and delete runs, answer approvals, pair and remove devices, push notifications, see which secrets exist |
| owner | everything: versions, promotions and rollbacks, secrets, roles, sessions, the audit log, skills, lessons, knowledge, conversations, workspace files |

A person's role comes from `roles` at each sign in (`owners`, then the highest
of `groups`, then `default_role`), unless an owner set one in the console
(`PUT /v1/console/members/{subject}`), which applies at once, also to sessions
that are open. `none` keeps someone out and ends their sessions, `null` goes
back to the mapped role. The owners of the config are owners whatever the
console says, so there is always a way back in. The last owner cannot be
demoted.

The role is checked on every route of the server, the ones that existed before
the console too. A route that a later version adds is closed to everyone but
owners until it is given a role. A signed in browser then goes on as the API
key would: the routes behind the console know no sessions, and the key never
reaches the browser. Webhook deliveries (`POST /v1/hooks/{hook}`) are refused
to browsers, they are for the systems that send them.

The API key is an owner. Device tokens (`--devices`) keep their scopes and
end user tokens (`--verify-jwt`) keep what end users may.

## Audit log

What is written:

| Action | When |
| --- | --- |
| `auth.sign_in`, `auth.sign_out`, `auth.session_end` | a sign in (also refused ones, with the person's groups), a sign out, an owner ending a session |
| `member.role` | an owner set or cleared someone's role |
| `secret.set`, `secret.delete` | the name only, never the value |
| `registry.upload`, `registry.deploy`, `registry.promote`, `registry.rollback` | with the version, its hash and the environments |
| `server.restart` | an owner applied changes |
| `approval.resolve`, `device.pair`, `device.revoke`, `run.abort`, `run.resume`, `run.delete`, `skill.change`, `learning.change`, `knowledge.change`, ... | the server's own routes, by signed in people and by bearer tokens |
| `run.start`, `run.feedback`, `push.*` | by signed in people (runs started with a bearer token are in the run store) |
| `audit.export`, `audit.retention` | an export, the daily removal of old entries |

A request refused for a missing role is written with `outcome: denied`.
Each entry is a JSON line:

```json
{"seq":42,"at_ms":1800000000000,"actor":{"kind":"user","id":"00u1...","email":"alice@example.com","role":"owner"},
 "action":"registry.promote","target":"support","outcome":"ok",
 "details":{"version":3,"hash":"9f2c...","from":"dev","to":"prod"},"prev":"51ab...","hash":"c04d..."}
```

`prev` is the `hash` of the entry before and `hash` the SHA-256 of the entry
itself, so an edited or removed line breaks the chain.
`GET /v1/console/audit/verify` checks it. Appends take a file lock, so servers
that share the data directory write one chain. Retention removes whole days
older than `audit.retention_days`, once a day.

`GET /v1/console/audit` filters by `since_ms`, `until_ms`, `actor` (id, email
or kind), `action` (exactly, or a group with a trailing dot: `secret.`),
`target` and `outcome`, newest first, `limit` up to 1000, with `before_seq`
for the next page. `GET /v1/console/audit/export` takes the same filters and
answers JSON lines, oldest first, for a SIEM or an archive.

## Secrets

`PUT /v1/console/secrets/OPENAI_API_KEY` with `{"value": "..."}` sets an
environment variable of the served workflows. The value is sealed with
AES-256-GCM under the console key, with a fresh nonce and the variable's name
as associated data, and is never answered again: the console lists names,
when they changed and by whom, and which served workflows declare them.

Secrets reach the workflows when the server starts, like `--env-file` entries,
and win over the process environment. Names that start with `GRAPHRT_`
configure the server itself and are refused. Keep the console key out of the
data directory (an environment variable or a mounted secret file): with
another key the server refuses to start rather than run without its secrets.

## Agent registry

`POST /v1/console/workflows/{name}/versions` with the workflow file as the body
(YAML, JSON or a `.grt` bundle) keeps a new version. The server compiles it
first, as it would serve it, and refuses one that does not compile. Versions
are numbered per workflow and kept once by their SHA-256: the same file again
is the same version. An upload goes to the first environment unless
`?environment=` names another (or `none`), and `?note=` describes it.

- `POST .../promote` with `{"from": "dev"}` runs in the next environment
  what `dev` runs (`"to"` names another).
- `POST .../rollback` with `{"environment": "prod"}` goes back to the version
  `prod` ran before (`"version"` names one). A second rollback undoes the first.
- `POST .../deploy` with `{"version": 3, "environment": "prod"}` runs a version.
- `GET /v1/console/workflows` lists what this server serves (with the hash of
  each file and its registry version), every version, what each environment
  runs with its history, and the changes that wait for a restart.

A server serves the registry version of its own `environment` for each
workflow that has one, in place of the file of the same name in the served
directory or next to it. Workflows without a registry version are served from
the directory as before. Servers of different environments can share one data
directory (a shared volume), or each keep their own.

## Applying changes

Secrets and versions take effect when the server starts. `POST
/v1/console/apply` (owner) does that: the server drains like on `SIGTERM`
(no new runs, active runs pause), then starts again with the same command, in
the same process on Unix, so a supervisor or a container sees no exit. Paused
runs carry on after the restart when the run store is a directory (`--store`).
The listener is closed for the moment of the restart, so clients should retry.

## API

| Method | Path | Role | What |
| --- | --- | --- | --- |
| GET | `/console/login` | – | start a sign in (`?return_to=/monitor/`, a path of this server) |
| GET | `/console/callback` | – | where the provider sends the browser back |
| GET | `/v1/console/session` | – | who is signed in, the role, the CSRF token, the environments |
| POST | `/v1/console/sign-out` | viewer | end this session |
| GET | `/v1/console/workflows` | viewer | served workflows, versions, environments, pending changes |
| POST | `/v1/console/workflows/{name}/versions` | owner | upload a version |
| GET | `/v1/console/workflows/{name}/versions/{version}/file` | owner | download a version |
| POST | `/v1/console/workflows/{name}/deploy`, `/promote`, `/rollback` | owner | change what an environment runs |
| GET | `/v1/console/secrets` | operator | names, never values |
| PUT, DELETE | `/v1/console/secrets/{name}` | owner | `{"value": "..."}` |
| GET | `/v1/console/members` | owner | everyone who signed in, their roles, the mapping |
| PUT | `/v1/console/members/{subject}` | owner | `{"role": "operator"}` (`owner`, `operator`, `viewer`, `none`, `null`) |
| GET, DELETE | `/v1/console/sessions`, `/v1/console/sessions/{id}` | owner | open sessions, end one |
| GET | `/v1/console/audit`, `/audit/export`, `/audit/verify` | owner | entries, JSON lines, the chain check |
| POST | `/v1/console/apply` | owner | restart to apply changes |

The console API takes a session or the API key (`Authorization: Bearer`),
not device or end user tokens. Answers are JSON with `Cache-Control: no-store`,
errors are `{"error": "..."}`.

## Security notes

- The identity provider must be reachable from the server (discovery, keys,
  token endpoint). It is contacted at the first sign in, not at startup, so a
  provider that is down does not stop the server, and the API key still works.
- Only verified emails count (`email_verified` not `false`) for `owners` and
  `allowed_domains`. Prefer subjects or groups for owners on providers that let
  users choose their email.
- CORS (`--cors-origin`) never allows credentials, so other origins cannot use
  a session.
- A secret's value is decrypted only at startup and given to the workflows.
  Workflows that print their environment can still show it: declare secrets as
  `type: secret` in the workflow so they are masked in events and the store.
