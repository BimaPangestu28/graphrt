- Admin console for self hosted servers (new `graphrt-console` crate, cargo
  feature `console`, `graphrt serve --console console.yaml`, off unless
  given): people sign in with OpenID Connect (Entra ID, Google Workspace,
  Okta, Keycloak, PKCE and nonce, sessions in `HttpOnly` `SameSite=Strict`
  cookies with CSRF tokens and a `Host` allowlist), get the role owner,
  operator or viewer from their groups or from an owner, and are held to it
  on every route of the server. An append only, hash chained audit log
  records sign ins, role changes, secrets, versions, approvals and devices,
  with filters, a JSON lines export, a chain check and retention. Secrets are
  environment variables of the served workflows, sealed with AES-256-GCM
  under a key from the environment or a file and never shown again. The
  agent registry keeps every version of a workflow by content hash, per
  environment (`dev`, `prod`), with promotion and rollback, and
  `POST /v1/console/apply` restarts the server with a drain so changes take
  effect. The API key keeps working for automation. See `docs/console.md`.
