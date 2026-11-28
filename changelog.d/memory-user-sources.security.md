- Long term memory keeps users of different sources apart. The user of a
  memory scope is now `<source>/<id>`: `user/<sub>` for a token the server or
  its gateway verified (and chat guests), `api/<id>` for an id named with the
  API key or by the host, `channel/<channel>:<id>` for a messaging channel
  customer. Before, a Telegram customer, a login and a backend user with the
  same id text could read each other's notes. Hosts set `sys.user_source` over
  anything a client sends, and subagents inherit it. Notes kept under the id
  alone move to the scoped id on the first run of that user (audit line
  `adopt_user`), owner reads show them until then, and erasing a scoped id
  erases them there too. The owner API
  and `graphrt memory --user` take scoped ids, and an id without a source means
  `api/<id>`. See `docs/memory.md`.
