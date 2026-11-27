- `llm` and `agent` nodes keep a long conversation inside the model's context
  window. With `context_window: auto` (the default) a request that the
  provider refuses for its size is trimmed and sent once more instead of
  failing the node: results of old tool calls become placeholders, then the
  oldest turns are removed whole, and a tool call is never parted from its
  result. `context_window: summarize` also carries a rolling summary of what
  was removed, `max_tokens` trims before a request passes a set limit, and
  `off` keeps the previous behaviour. There is no table of model sizes: the
  window is read from the provider's refusal. Each trim is reported as a
  `context_trimmed` progress event. In the default build (cargo feature
  `context`), not in the slim builds. See "Context window" in
  `docs/workflow-format.md` and `docs/memory-design.md`.
