- `graphrt collab` (cargo feature `collab`, image `:<version>-collab`): a
  collaboration server where teams build agents together in real time. Team
  workspaces with the console's roles (viewer, operator who can edit, owner),
  invites by link, live co-editing of the graph, prompts and layout that merges
  without conflicts (a Yjs compatible CRDT), presence, offline edits that merge
  on reconnect, automatic and named versions with restore, an audit log and
  secrets per workspace. Sign in with OpenID Connect (a loopback redirect for
  desktop apps) or with invite codes. Short lived access tokens scoped to a
  workspace, rotating refresh tokens, rate limits, TLS. Hosted (many teams)
  or self hosted (one company) from the same binary, with a compose file and a
  Helm chart in `deploy/collab/`. See docs/collab.md and docs/design/collab.md.
- `graphrt-collab`: the document model, the sync protocol, the server and a
  client for apps (Graphrt Desktop uses it).
- Plugin manifests: optional `services`,
  server side services a collaboration server runs for workspaces that enable
  them, sandboxed like every plugin, with workspace secrets an owner grants.
