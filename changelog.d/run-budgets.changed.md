- Breaking for code that builds these structs with literals: `Limits` and
  `RunInput` gained `max_total_tokens` and `max_cost_micros`, `Snapshot`
  gained `budget`, `NodeSnapshot` gained `checkpoint`, `ToolCallContext`
  gained `idempotency_key`, the plugin SDK's `ToolCall` gained
  `idempotency_key`, and a providers entry may hold `prices`. All have
  defaults (`..Default::default()`), and snapshots written before keep
  working.
