- `human-input`: `on_expire` says what a host should do when the request
  expires (`expire_after_ms`): `stop` (the default) or `{ action: <id> }` to
  answer with one of the node's actions and the fields' defaults. It is
  checked against the actions and reported in the pause data as `on_expire`
  (`{"kind": "stop"}` or `{"kind": "answer", "action": "<id>"}`) next to
  `timeout_ms`. graphrt does not expire requests itself (`run`, `serve` and the
  gateway keep the run paused); hosts that enforce deadlines carry it out.
