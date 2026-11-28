- Agents deployed online run their subagents, budgets and memory. `graphrt
  bundle build` adds the workflows the agents list as `subagents` (and what
  those list) under `workflows/`, with their skills and plugins, from the
  entry's folder or `--workflows DIR`, and writes `--max-tokens-per-run` and
  `--max-cost-per-run` (which needs `--providers`, whose prices count) into
  the manifest's new `limits`. A bundle with `limits` is format 2, so older
  runtimes refuse it instead of running it without budgets. `graphrt serve`
  and `graphrt run` link a bundle's workflows to its entry only (they get no
  endpoint of their own) and apply its budgets, the lower of the host's and
  the bundle's. `graphrt deploy --direct` gives an agent with
  `long_term_memory` the store and `--memory-dir /data/memory` on Fly and
  Cloud Run, reports `"memory"` in the result, refuses it on Lambda, and
  refuses a workflow file whose agents list subagents (deploy a bundle).
  `graphrt deploy capabilities` says per target whether memory is kept, and
  what the agent file carries (`agent_file`).
- `graphrt run --workflows DIR` keeps a subagent's memory under its file
  name, as `graphrt serve` does, not under its `name:`. A bundle's subagent
  keeps it under `<bundle>.<name>`.
