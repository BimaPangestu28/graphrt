- `agent` nodes remember users across runs with `long_term_memory: auto`:
  the tools `memory/remember`, `memory/recall` and `memory/forget` over notes
  kept per tenant, agent and user in files (new crate `graphrt-memory`, no
  database), and matching notes added to the request as data when a run
  starts. Every write passes a gate: secrets are masked, notes built from tool
  results or web pages and text that reads like an instruction wait for
  review, and every change is in an audit log without note text. The owner
  lists, approves, exports and erases with `/v1/memory` and `graphrt memory`,
  a verified user with `/v1/me/memory`. `--memory-dir` and `--memory-tenant`
  on `run` and `serve`. In the default build (cargo feature `memory`), not in
  the slim builds. See `docs/memory.md`.
- What an agent's tools did in a turn reaches the next turn of a
  conversation: the `agent` node outputs `turn_digest`, `serve
  --conversations` stores it and sends it in `sys.history` as `tool_digest`,
  framed as a record written by the runtime.
