- `agent` nodes can start other workflows as tools with `subagents:
  [{workflow, description, handoff?}]`: the model picks which listed
  workflow to call, how often and several at once (`max_parallel_subagents`,
  `max_subagent_calls`). Each call is a child run of the same run with a
  context of its own, the same step budget, deadline, depth limit, services
  and secrets, and its events in the same stream under frame
  `<node>.<workflow>[<n>]`. An approval inside a child pauses the whole run
  and resuming it continues the child. `handoff: true` makes the child's
  answer the agent's answer. `graphrt serve` links the workflows it serves,
  `graphrt run --workflows DIR` the files of a directory, and a snapshot is
  not resumed against a changed child workflow. In the default build (cargo
  feature `subagents`), not in the slim builds. See `docs/workflow-format.md`
  and `docs/orchestration-design.md`.
