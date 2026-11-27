# Agents that call workflows, budgets and checkpoints: design

This page records how a Graphrt agent hands work to other workflows, how a
run is bounded in tokens and cost, and how the agent loop survives a retry, a
timeout or a preempted run without repeating what it already did. It is the
second half of the work described in [memory-design.md](memory-design.md)
(stages 4 and 5).

Decided 2026-11-27 against `main` at `ab75abe`.

## What the runtime already had

| Existing piece | Where | Gap |
| --- | --- | --- |
| Child frames for container nodes (`Subgraph`, `ScopeRunner`), with the scheduler, the event sink, the step budget and the control signals of the run | `graphrt-core::engine::frame` | A child frame runs a body of the same graph. No way to run another workflow. |
| Pause inside a child reaches the root: the container saves the child's `FrameSnapshot` and the reasons carry the frame (`review[2]/approve`) | `graphrt-core::nodes::iteration`, `engine` | Only for containers. A tool cannot pause. |
| Step budget shared by every frame, kept across resume (`Limits::max_steps`) | `engine` | One `agent` node is one step whatever it does, so the loop is bounded only by `max_iterations`. No bound on tokens or cost. |
| Deadline per attempt and per run, depth limit of 8 | `engine` | none, reused |
| Usage per node, summed per frame and per run | `engine`, `event::Usage` | Reported after the node ends. Nothing can stop a run for it. |
| `AgentState`, saved when a tool waits for approval | `graphrt-llm::nodes::agent` | Saved in that one case. A retry or a resumed preempt starts the loop from turn 1 and calls tools again. |
| `NodeContext::idempotency_key` (`<run_id>:<exec_id>`, stable across attempts) | `graphrt-core::node` | Per node, not per tool call. |
| `iteration`: one body run per item, up to 100 in parallel | `graphrt-core::nodes::iteration` | The model can decide how many, not which kind. |

## Stage 4: workflows as tools

### What the owner writes

```yaml
- id: lead
  type: agent
  data:
    model: {provider: openai, name: gpt-5}
    subagents:
      - workflow: research          # a workflow this host serves or bundles
        description: Researches one question on the web and returns a cited answer.
      - workflow: write-report
        description: Writes a report from notes.
        handoff: true               # its answer becomes the answer of this agent
    max_parallel_subagents: 4       # default 4
    max_subagent_calls: 16          # default 16 per run of this node
```

Each entry becomes a tool the model sees, named after the workflow, with the
workflow's start variables as its parameters. The model chooses which to call,
how many times, and may call several in one turn. That is the dynamic part.
The list is the allowlist: a model cannot name a workflow that is not in it,
and a host only resolves workflows it was started with (`serve` workflows, the
files of a bundle, `graphrt run --workflows DIR`).

Cargo feature `subagents` (`graphrt-core`, `graphrt-llm`, `graphrt-cli`). In
the default release build, not in `agent-slim` or `agent-minimal`.

### Why not `ToolRuntime`

The audit suggested a tool provider that runs a workflow and refuses a child
that needs approval. That was rejected for three reasons:

1. `ToolRuntime::call` returns a result or an error. It cannot pause, and the
   owner requires that an approval inside a child really pauses and resumes up
   to the parent.
2. A run started inside `call` would be a second run: its own step counter,
   its own deadline, its own secret set and events that the parent's stream
   never sees.
3. Changing the contract of `ToolRuntime` to return "paused with a snapshot"
   would touch every provider (plugins, MCP, files, images, documents, wasm)
   for a case only one of them has.

The engine already solves all of this for containers. So the contract of
`ToolRuntime` stays as it is, and an agent with `subagents` runs its children
through the engine, the way `iteration` runs its body.

### How it runs

- **Linked graphs.** A run gets, beside its own graph, the graphs of the
  workflows its agents list (`RunInput` carries them, the host builds them once
  at start). A frame holds the graph it executes instead of assuming the run's
  graph. The snapshot fingerprint covers the linked graphs, so a run cannot be
  resumed against a changed child.
- **Child frames.** A node with `subagents` receives a `ChildRuns` handle in
  its `NodeContext`. `run(workflow, inputs, call_index, resume)` executes the
  linked graph as a child frame `"<frame>/<node>.<workflow>[<n>]"` of the same
  run and returns completed, failed or paused, like `SubgraphResult`.
- **A separate context.** The child's pool starts empty. It gets the tool
  arguments as start inputs (checked against the start variables) and the
  `sys` values that identify the request (`run_id`, `user_id`,
  `conversation_id`). It does not see the parent's messages, variables or
  query. Its answer text or `end` outputs come back as the tool result, cut by
  `max_tool_output_chars` like any result.

### What a child inherits

| From the parent | How |
| --- | --- |
| Step budget | The same counter. Every node attempt of a child counts. |
| Token and cost budget (stage 5) | The same counters. |
| Deadline | The child runs inside the parent node's attempt, so its timeout bounds it, and `max_duration_ms` of the run is one timer. |
| Depth | A child frame is one level deeper. `max_depth` (8) covers containers and children together, so a workflow that lists itself ends at the limit instead of recursing forever. |
| Egress policy, file workspace, plugin and MCP registries | The same `Services`. A child reaches nothing the host did not give the run. |
| Secrets | The same secret set of the run, so anything a child emits is scrubbed. Environment values are the ones the host was started with. The child document cannot add any. |
| Abort, pause and preempt | The same control signals. |
| Events | The same sink, with the child's frame id, so one stream shows the whole tree. |

Not inherited: the parent's tools, skills, instructions and memory scope. A
child uses what its own document lists. `tool_context` values are passed only
when the entry says so (`pass_context: [provider]`).

### Approval inside a child

A child that pauses (a tool that needs approval, a `human-input` node) returns
its frame snapshot and reasons. The agent keeps the snapshot in its state under
the call id, finishes the other calls of the turn, and pauses with the child's
reasons. The run pauses with keys such as `lead.research[0]/assistant`. On
resume the agent restores the child frame with the host's input and the loop
goes on when the child completes. The approvals inbox and the frame qualified
resume keys work unchanged.

### Parallel calls and limits

- Calls to subagents in one turn run concurrently, at most
  `max_parallel_subagents` at a time (and never above `Limits::max_concurrency`).
- `max_subagent_calls` bounds the calls of one node run. Further calls get an
  error result that tells the model the limit, so it can finish with what it has.
- A failed child is a failed tool call: the model reads the error and decides.
  It does not fail the parent unless the run's budget or deadline is gone.

### Handoff

An entry with `handoff: true` is a transfer. When the model calls it, the
child's answer is streamed as the answer of the parent node and the loop ends
(`stopped_reason: handoff`). It is the cheap form of "pass the conversation to
a specialist": no second model turn to repeat what the child said. Only one
handoff per turn is accepted, and other calls of that turn run first.

### Tests

- An agent picks between two listed workflows, and calls one three times in
  one turn with the parallel bound observed.
- A workflow that is not listed cannot be called (the name is not a tool).
- Step budget, depth and deadline: a child that loops ends the run at the
  shared limit. A workflow that calls itself stops at `max_depth`.
- A tool approval inside a child pauses the run, the snapshot survives JSON,
  resume continues the child and then the parent, and every tool ran once.
- The child does not see the parent's variables or messages. Secrets printed
  by a child are scrubbed in the parent's result.
- An agent inside `iteration` that calls a subagent.

## Stage 5: budgets and checkpoints

### Token and cost budget

```rust
pub struct Limits {
    // existing fields
    /// Total tokens (prompt and completion) the run may use, children included.
    pub max_total_tokens: Option<u64>,
    /// Cost the run may reach, in millionths of the price unit of the providers file.
    pub max_cost_micros: Option<u64>,
}
```

- **Charging.** `NodeContext::charge(&Usage, cost)` adds to counters of the
  run (atomics, shared by every frame and child) and returns an error when a
  limit is passed. Model nodes call it after every model response, so an agent
  loop is charged per turn, not when the node ends. The counters are in the
  snapshot and continue after resume.
- **Cost.** There is no price list in the binary, prices change too often.
  The providers file may give `price: {input_per_million, output_per_million}`
  per model. Without it only the token limit applies.
- **Running out.** Before a model call the node checks what is left. An agent
  that cannot afford another tool turn makes one last turn without tools, the
  way `max_iterations` ends (`stopped_reason: budget`). When nothing is left
  the node fails with `limit_exceeded` and the run stops like at the step
  limit.
- **Where it is set.** `--max-tokens-per-run` and `--max-cost-per-run` on
  `run` and `serve`, and per request up to the server's value.

### Checkpoints

```rust
impl NodeContext {
    /// Keep `state` as this node's progress. The next attempt of the same
    /// execution (a retry, or a resume after preempt) receives it.
    pub fn checkpoint(&self, state: serde_json::Value);
    /// The checkpoint of an earlier attempt, if any.
    pub fn checkpointed(&self) -> Option<&serde_json::Value>;
}
```

- The engine keeps the latest checkpoint of a running node in memory. A retry
  in the same process gets it. When the run is preempted or paused elsewhere,
  the snapshot stores it in `NodeSnapshot::resume_state` with the node still
  `pending`, so the resumed run hands it back.
- It is separate from `Resume` (the state of a node that paused itself), so
  a node can tell "continue after approval" from "continue after an interrupted
  attempt".
- The checkpoint is cleared when the node completes.

The agent loop writes a checkpoint at three points:

1. after a model turn, with the calls it is about to run marked as started
2. after each tool result
3. after a child run completes or pauses

On a new attempt the loop continues from the checkpoint. Model turns that were
answered are not asked (and not paid for) again. Tool results that were
recorded are not run again.

### A call that was started and has no result

That is the one case where the outcome is unknown: the tool may or may not
have acted. The rule:

| The tool is | On the next attempt |
| --- | --- |
| Listed in `retry_safe_tools`, or one of the read only built in tools (`memory/recall`, file reads, `load_skill`) | Called again. |
| Anything else | Not called. The model gets the result `interrupted: this call was started and its outcome is unknown. It was not repeated.` and decides what to do, which may be to check and call again. |
| Listed in `require_approval` | Not called. It waits for approval again, with a note that an earlier attempt was interrupted. |

Every tool call also receives an idempotency key
(`<run_id>:<exec_id>:<call id>`) in its `ToolCallContext`, the same for every
attempt of that call, so a plugin or an HTTP service can make the effect happen
once even when the owner marks the tool as safe to retry.

### Resume outside approval

With checkpoints, a preempted run (a server that drains, a gateway that moves
an agent) resumes in the middle of an agent loop, not at its first turn. The
same holds for a node timeout followed by a retry.

### Tests

- A retry after a retryable model error in turn 3 sends two fewer model
  requests than a fresh loop and calls no tool twice.
- A timeout in the middle of a tool call: the call is not repeated, the model
  sees `interrupted`, a tool in `retry_safe_tools` is repeated.
- Preempt during a loop, snapshot through JSON, resume: the loop continues.
- A token budget ends an agent with a final answer. A child that spends the
  parent's budget stops the whole run. The counters survive resume.
- Cost follows the prices of the providers file.

## Threat model

| Threat | Decision that holds against it | Test |
| --- | --- | --- |
| A model starts workflows the owner did not intend | Allowlist per agent. Only workflows the host was started with resolve. | Unlisted name is not a tool. |
| A model starts children without end (fork bomb, recursion) | `max_subagent_calls`, `max_parallel_subagents`, shared steps, tokens, deadline and depth. | Limits tests above. |
| A child is used to get around the parent's approval rules | A child has its own tools and its own `require_approval`, and the pause reaches the root. A child cannot call the parent's tools. | Approval inside a child. |
| A child leaks the parent's context | Fresh pool, arguments only. | Isolation test. |
| A child reaches the network or secrets the run may not | Same services and secret set, no new environment values. | Secret scrub test. |
| Injected text in a child's answer steers the parent | The answer is a tool result: bounded, masked, and data to the model like any tool result. A handoff is opt in per entry. | Covered by the tool result tests. |
| A retry repeats a payment or a message | Checkpoints, the unknown outcome rule, idempotency key per call. | Timeout tests above. |
| A run spends without bound | Token and cost limits charged per turn across the tree. | Budget tests above. |
| A resumed run continues against a changed workflow | The fingerprint covers linked graphs. | Resume with a changed child is refused. |

## Performance targets

| What | Target |
| --- | --- |
| Starting a child frame of a linked graph (graph built at start) | under 100 µs |
| `charge` per model turn | two atomic additions |
| Checkpoint of a loop with 200 messages | under 1 ms |
| A run without subagents, budgets or checkpoints | no measurable change |
| Binary size, default build | `subagents` under 120 KiB. Budgets and checkpoints are in the engine, under 40 KiB, and need a decision on the slim budgets because they cannot be left out of a build that has agents |
| Binary size, `agent-minimal` | under 10 KiB (the limit fields and counters) |

The last two rows are the one place where the slim builds may grow. The pull
request of stage 5 measures it. If it does not fit the current budgets the
checkpoint code goes behind a feature and the slim builds keep the behaviour
they have today.

## What each stage delivers

| Audit point | Before | After |
| --- | --- | --- |
| 2a. An agent calls another agent or workflow | Not possible | Stage 4: `subagents`, child frames, handoff |
| 2b. The model decides how many and which | Count only | Stage 4: the model picks from the allowlist, several at once |
| 2c. Token and cost budget | None | Stage 5: `Limits`, charged per turn across the tree |
| 2c. Approval inside a subagent | Tested with `human-input` only | Stage 1 (agent inside `iteration`) and stage 4 (inside a child) |
| 2c. Snapshot and resume of the loop | Only at approval | Stage 5: checkpoints per turn |
| 2c. A retry repeats tools with effects | Yes | Stage 5: no |

## For the desktop app

- Node keys: `subagents`, `max_parallel_subagents`, `max_subagent_calls`,
  `retry_safe_tools`.
- Run limits: `max_total_tokens`, `max_cost_micros` per request and per server.
- Events carry the child frame id, so a run view can draw the tree. New
  progress events: `subagent_started`, `subagent_finished`, `budget`.
- Pause reasons of children arrive with frame qualified keys, which the
  approvals inbox already shows.
