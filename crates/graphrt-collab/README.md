# graphrt-collab

Real time collaboration on [Graphrt](https://github.com/Graphrt/graphrt) agent projects.

- `doc`: the shared document of a project, a Yjs compatible CRDT (`yrs`).
  `ProjectDoc::apply_json` turns a new version of the project JSON into the
  smallest update, so apps keep saving JSON and edits merge: different nodes,
  the same prompt typed in by two people, offline edits.
- `protocol`: the sync messages (the Yjs sync protocol plus Graphrt control messages).
- feature `server`: the collaboration server behind `graphrt collab serve`:
  team workspaces with the console's roles, invites, sign in with OpenID
  Connect or invite codes, live rooms with presence, versions, an audit log
  and secrets per workspace, rate limits, TLS. Hosted or self hosted.
- feature `services`: plugin services (graphrt-plugin manifests with `services`).
- feature `client`: `Api` (check a server, sign in, workspaces), `LiveProject`
  (a local replica kept in sync, reconnecting, offline edits), `LoopbackLogin`
  (the browser sign in of a desktop app).

Guide: [docs/collab.md](https://github.com/Graphrt/graphrt/blob/main/docs/collab.md).
Design: [docs/design/collab.md](https://github.com/Graphrt/graphrt/blob/main/docs/design/collab.md).

Apache-2.0.
