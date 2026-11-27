- `graphrt gateway` no longer passes `resume`, `event_index`,
  `preempt_after_ms`, `fingerprint` or `deployment_version` from a caller's
  run request to the agent. Only the gateway sets them, so a caller cannot
  resume a run from a snapshot of its own (forged node state, a budget set
  back to zero).
