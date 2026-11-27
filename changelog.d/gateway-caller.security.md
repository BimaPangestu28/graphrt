- `graphrt serve --trust-gateway` no longer takes `sys.user_id` from a run
  request unless the gateway says it verified that user. The gateway now sets
  `caller` in the run request it signs (`user` for a verified end user, `key`
  otherwise). A request without it, from a gateway older than this release,
  forwarded the client's `sys` as it came, so a user could name another user
  and read their lessons. Such a request now runs with no `sys.user_id` (no
  lessons or files per user) and the agent logs a warning once. Upgrade
  note: upgrade `graphrt gateway` together with its agents, or first, to keep
  lessons per user working. A run started by a gateway older than this
  release cannot be rated as a user's own words through the new feedback
  route (`403`).
