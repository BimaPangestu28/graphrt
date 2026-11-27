- `graphrt serve --cors-origin ORIGIN` (repeatable, or `$GRAPHRT_CORS_ORIGINS`):
  pages of the listed origins may call the API from a browser. Off by default.
  Origins are matched exactly, `Access-Control-Allow-Credentials` is never sent,
  and `*` needs an API key. See [docs/web.md](docs/web.md#cross-origin-requests-cors).
- `graphrt serve --chat WORKFLOW[=INPUT]`: a chat page at `/chat/` and a chat
  widget for other sites (`<script src=".../chat/embed.js">`), embedded in the
  binary, for phones and desktops, light and dark. `--chat-access key|users|guests`
  decides who may chat. Guests get a signed, stateless session and are limited by
  `--chat-guest-rate-limit` and `--chat-guest-budget`. `--chat-mermaid FILE`
  draws `diagram` blocks with a Mermaid build you name, served from your server.
- `graphrt serve --conversations`: the server keeps chat history. A run names a
  `conversation_id` instead of sending `sys.history` (which keeps working), and
  gets the earlier turns and the conversation variables. Scoped to the verified
  user, with `--conversation-max-messages`, `--conversations-per-user` and
  `--conversation-ttl-days`. API under `/v1/conversations`.
- A verified end user (`--verify-jwt`, or a chat guest) can read, follow and stop
  the runs they started: `GET /v1/runs/{id}`, its events and `abort` accept the
  user's token for their own runs, filtered like their stream. `--user-resume`
  also lets them answer the questions of their own paused runs.
- `GET /v1/runs/{id}/events?responses=true` adds the answer text (`response`
  events) for a caller that follows a run, and the `202` answer of an async
  start or resume has `first_event`.
- `web/`: browser packages built on the above. `graphrt-client` (typed client:
  blocking, streamed and background runs, events with replay, abort, resume,
  feedback, conversations), `graphrt-ui` (web components that draw UI blocks,
  checked against the `graphrt-ui` fixtures) and `graphrt-chat` (the chat
  component and widget).
