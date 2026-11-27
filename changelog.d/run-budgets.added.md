- A run can have a token and a cost budget: `--max-tokens-per-run` and
  `--max-cost-per-run` on `run` and `serve`, `limits` in a run request of
  `serve` (only lower than the server's), `max_total_tokens` and
  `max_cost_micros` in `Limits` and in the FFI limits. Model turns are charged
  as they happen, child runs included, with prices per model from `prices` in
  the providers file. An agent that cannot afford another tool turn answers
  without tools (`stopped_reason: budget`). The counters are in the snapshot.
- The `agent` loop keeps a checkpoint per turn: a retry, a node timeout or a
  preempted run that is resumed goes on from the last turn instead of
  starting over, and a tool that already ran is not run again. A call cut
  while it ran is repeated only when it is read only or listed in
  `retry_safe_tools`, otherwise the model reads that its outcome is unknown.
  Every tool call carries an idempotency key, which plugins receive as
  `idempotency_key`. See `docs/workflow-format.md` and
  `docs/orchestration-design.md`.
