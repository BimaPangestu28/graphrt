- The admin console page at `/console/` (`graphrt serve --console`): agents
  with the version each environment runs, uploads, promotion, rollback and
  "Restart and apply", runs and approvals through the monitoring view,
  secrets by name, people with their roles and open sessions, and the audit
  log with filters, the JSON lines export and the chain check. What each
  person sees follows their role, at phone width too. The monitoring page
  uses a console sign in when there is one and offers "Sign in with your
  organization" next to the key. The browser package gains `@graphrt-dev/graphrt/console`, `client.console`
  and `sessionFetch`.
