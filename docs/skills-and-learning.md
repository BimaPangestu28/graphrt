# Skills and self learning: research and design

**Status: design, preview.** Nothing on this page changes how Graphrt behaves
today. It records what was researched, what Graphrt already has, the threats
that are specific to these two features, and the decisions the implementation
follows. Each decision has its reason next to it.

Two capabilities are in scope:

1. **Skills.** An agent can be given a skill: a package of instructions plus
   optional reference files and tool requests, loaded only when a task needs
   it. A new ability is added by attaching a skill, not by rewriting the agent.
2. **Self learning.** An agent gets better from its own use: run results, user
   corrections and patterns that repeat become short lessons that later runs
   read.

Four requirements shape every decision: modular, fast, secure, and usable by
people who do not write code or YAML.

## 1. What other systems do

Sources were read directly: the two specifications from their repositories,
papers from their arXiv abstracts, and vendor documentation pages. Where a page
could only be read in part, that is said.

### 1.1 Skill formats

**Agent Skills (`SKILL.md`).** The specification at
[agentskills.io](https://agentskills.io/specification)
([repository](https://github.com/agentskills/agentskills)) defines a skill as a
directory with a `SKILL.md` file: YAML frontmatter followed by Markdown.

| Frontmatter field | Rule in the specification |
| --- | --- |
| `name` | Required. 1 to 64 characters, `a-z`, `0-9` and `-`, no leading, trailing or double hyphen, equal to the directory name. |
| `description` | Required. 1 to 1024 characters. Says what the skill does and when to use it. |
| `license`, `compatibility` | Optional. `compatibility` is at most 500 characters. |
| `metadata` | Optional map of string keys to string values, for client specific data. |
| `allowed-tools` | Optional and marked experimental. A space separated list of tools that are approved in advance. |

Optional directories are `scripts/` (code the agent may run), `references/`
(documents read on demand) and `assets/` (templates and data).

The format is built around **progressive disclosure** in three tiers:

| Tier | What is loaded | When | Cost |
| --- | --- | --- | --- |
| 1. Catalog | `name` and `description` | Session start | About 50 to 100 tokens per skill |
| 2. Instructions | The `SKILL.md` body | When the skill is activated | Under 5000 tokens recommended |
| 3. Resources | Files under the skill directory | When the instructions point at them | Varies |

The client implementation guide in the same repository adds rules that matter
for Graphrt:

- A client whose model cannot read files uses a **dedicated activation tool**
  that takes a skill name and returns the body. The guide recommends
  constraining the name to the set of valid skills, wrapping the returned body
  in identifying tags, listing bundled files without reading them, and
  registering no tool at all when there are no skills.
- Parsing is lenient about cosmetic problems (a name that differs from its
  directory still loads) and strict about a missing description.
- Skills that are disabled or denied by a permission system are hidden from
  the catalog, not listed and then refused.
- Skills that arrive with untrusted content (a cloned repository) should be
  gated on a trust decision.

**Agent Plugins.** The specification at
[agent-plugins.org](https://agent-plugins.org)
([repository](https://github.com/agentplugins/agent-plugins-spec), versions
1.0.0 and 1.1.0 read) packages components into one directory:

```text
my-plugin/
  plugin.json        required manifest: $schema and name
  skills/<name>/SKILL.md
  mcp.json           MCP servers: stdio, Streamable HTTP, legacy HTTP+SSE
  hooks.json         added in 1.1.0
  <reverse.domain>/  client specific files
```

It does not redefine skills: section 7.1 says skills "MUST conform to the Agent
Skills specification" and only fixes where they are found (`skills/`, one level
deep). A client may support a single component type and still conform. Paths
must resolve inside the plugin root. A failure in one component must not stop
the others from loading.

What it leaves out is as important. Its `FUTURE_CONSIDERATIONS.md` states that
1.0.0 "does not define a trust model, permission system, or sandboxing
requirements" and "does not specify how clients or users can verify the origin
or integrity of a plugin". Permissions, signatures, secrets and audit trails
are each client's job.

**Vendor extensions.** Anthropic's documentation for the API surface repeats
the three tiers, forbids XML tags in `name` and `description`, and reserves
some words in names. Its security section says to use skills "only from trusted
sources", to audit every bundled file, and to "treat like installing software".
Claude Code documents `allowed-tools` as a grant for the turn that uses the
skill and not as a restriction (that page could only be read in part). The
field therefore has no shared meaning across clients, which is why the
specification still marks it experimental.

### 1.2 Memory and learning

The CoALA framework
([arXiv 2309.02427](https://arxiv.org/abs/2309.02427)) gives the vocabulary
most systems use:

| Memory | Holds | Example in a business agent |
| --- | --- | --- |
| Episodic | What happened in past runs | "On 12 March the refund for order 811 was declined" |
| Semantic | Facts | "Invoices go to finance@ and not to the account manager" |
| Procedural | How to do things | "Before quoting, check stock in the sheet" |

Approaches, grouped by what they change:

| Approach | Mechanism | Evidence |
| --- | --- | --- |
| Small curated memory that is always in context | A capped block of notes the agent or user edits. Letta memory blocks have a per block character limit, a read only flag and can be shared. Hermes Agent keeps `MEMORY.md` at 2,200 characters and `USER.md` at 1,375 and returns an error when full. ChatGPT lets users view, correct and delete saved memories and offers temporary chats that write none. | In production |
| Memory as files behind a tool | Anthropic's memory tool is client side: the model asks for file operations and the application runs them in its own storage. The documentation asks the application to validate paths, cap sizes and expire old files. | In production |
| Background formation | LangMem separates writing memory in the "hot path", which "adds perceptible latency to user interactions", from a background pass after the conversation. Hermes runs a review after each turn. | In production |
| Skills written by the agent | Voyager ([arXiv 2305.16291](https://arxiv.org/abs/2305.16291)) stored working code as a growing skill library. Hermes Agent ships the same idea with `SKILL.md` files written through a `skill_manage` tool. Approval of those writes exists but is off by default. | Paper, then production |
| Reflection in words | Reflexion ([arXiv 2303.11366](https://arxiv.org/abs/2303.11366)) keeps verbal reflections on failed attempts. Generative Agents ([arXiv 2304.03442](https://arxiv.org/abs/2304.03442)) synthesizes memories into higher level reflections. ExpeL ([arXiv 2308.10144](https://arxiv.org/abs/2308.10144)) extracts insights from a set of training tasks. | Papers |
| Reusable workflows from past runs | Agent Workflow Memory ([arXiv 2409.07429](https://arxiv.org/abs/2409.07429)) induces routines from past trajectories and reports large gains on web navigation benchmarks. | Paper |
| Evolving playbooks | Dynamic Cheatsheet ([arXiv 2504.07952](https://arxiv.org/abs/2504.07952)) keeps a curated memory of strategies at inference time. ACE ([arXiv 2510.04618](https://arxiv.org/abs/2510.04618)) names two failure modes of rewriting a whole context, "brevity bias" and "context collapse", and avoids them with small itemized updates. | Papers |
| Prompt optimization from feedback | GEPA ([arXiv 2507.19457](https://arxiv.org/abs/2507.19457)) reflects on trajectories to propose and test prompt changes. LangMem ships a prompt optimizer that takes trajectories with optional feedback. | Library level. Needs an evaluation set to be safe. |
| Virtual context management | MemGPT ([arXiv 2310.08560](https://arxiv.org/abs/2310.08560)) pages memory in and out of the context window. | Paper, then a product (Letta) |

What holds up in production is narrower than the literature: a **small, capped,
human readable memory**, formed **outside the request path**, that the user can
**see, correct and delete**, plus **procedures stored as skills**. Fully
automatic loops that rewrite an agent's instructions from its own output are
research results on benchmarks, with no owner looking at what was learned.

### 1.3 How these features get attacked

**Skill supply chain.** An academic study of 31,132 skills from two
marketplaces ([arXiv 2601.10338](https://arxiv.org/abs/2601.10338)) found 26.1%
with at least one vulnerability and reports that skills bundling executable
scripts are 2.12 times more likely to contain one. It concludes that the
ecosystem needs "capability-based permission systems and mandatory security
vetting". A scan of 3,984 marketplace skills by Snyk
([report](https://snyk.io/blog/toxicskills-malicious-ai-agent-skills-clawhub/),
[arXiv 2605.28588](https://arxiv.org/abs/2605.28588)) confirmed 76 malicious
payloads and critical issues in 13.4% of skills. Prompt injection appeared in
91% of the malicious skills and malicious code in all of them. Techniques
included instructions to download and run a payload, obfuscated commands that
send credential files, and loading further instructions from a remote URL. The
marketplace had no signing, no review and no sandbox by default.

**Memory poisoning.** MINJA
([arXiv 2503.03704](https://arxiv.org/abs/2503.03704)) writes malicious records
into an agent's memory by ordinary queries alone, so any user can influence
what later users get. MemoryGraft
([arXiv 2512.16962](https://arxiv.org/abs/2512.16962)) plants "successful
experiences" through content the agent reads, and the agent then imitates them
in later sessions. AgentPoison
([arXiv 2407.12784](https://arxiv.org/abs/2407.12784)) poisons a memory or
knowledge base with a trigger. A real product was hit the same way: a web page
caused ChatGPT to store instructions in its memory that then leaked data in
every later session
([Embrace the Red, 2024](https://embracethered.com/blog/posts/2024/chatgpt-macos-app-persistent-data-exfiltration/)).
The vendor closed the exfiltration channel. The author notes that untrusted
content could still write memories.

A follow up study ([arXiv 2601.05504](https://arxiv.org/abs/2601.05504)) found
that filters on memory content need a trust threshold that is hard to set: too
strict blocks everything and too loose misses subtle attacks.

**Guidance.** The OWASP Top 10 for Agentic Applications 2026 lists
[ASI06 Memory and Context Poisoning](https://genai.owasp.org/agentic-applications-2026/asi06-memory-context-poisoning/)
and
[ASI04 Agentic Supply Chain Vulnerabilities](https://genai.owasp.org/agentic-applications-2026/asi04-agentic-supply-chain-vulnerabilities/).
For memory it asks to scan writes before commit, to "prevent automatic
re-ingestion of an agent's own generated outputs into trusted memory", to
expire unverified memory, to use a namespace per tenant and to support rollback
and quarantine. For supply chain it asks to "sign and attest manifests, prompts,
and tool definitions", to pin by content hash and to have a kill switch.

Two design level results matter more than any filter. Simon Willison's
[lethal trifecta](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/)
(private data, untrusted content and a way to send data out) explains why
detection alone fails: a filter that catches 95% of attacks is "very much a
failing grade". CaMeL ([arXiv 2503.18813](https://arxiv.org/abs/2503.18813))
shows the alternative: keep untrusted data from deciding control flow, and
enforce capabilities where tools are called.

The lesson for Graphrt: **the boundary must be what a skill or a lesson is
able to do, enforced in code. Scanning text is a useful second layer and
nothing more.**

## 2. What Graphrt already has

| Building block | Where | Reuse |
| --- | --- | --- |
| Tool contract: `ToolRuntime`, `ToolRegistry`, tools named `provider/name` | `graphrt-core/src/tool.rs` | Skill activation and file reads are tools of a `skills` provider. No new protocol. |
| `agent` node with `tools`, `require_approval` and a pause of kind `tool_approval` | `graphrt-llm/src/nodes/agent.rs` | Tools that arrive through a skill ask first through the same pause the desktop app already renders. |
| Typed host services (`Services`) | `graphrt-core/src/node.rs` | Skills and lessons reach the agent node as two optional services. A host that registers neither pays nothing. |
| Plugins with declared `permissions`, operator trust, checksums verified before every start | `graphrt-plugin`, [plugins.md](plugins.md) | Same model: a package requests, the operator grants. Skill tools are plugin or MCP tools, so their sandbox and limits already apply. |
| MCP client and MCP presets with `approval.read_only` and `ask_first` | `graphrt-mcp`, [plugin-manifest.md](plugin-manifest.md) | A skill can request MCP tools by name. |
| Plugin catalog client: index, sha256 per tarball, safe extraction, confirmation for risky entries | `graphrt-cli/src/plugin_catalog` | Skills are published and fetched the same way. |
| Catalog signature | The graphrt-plugins release workflow signs the index with minisign and the desktop app refuses an index without a valid signature | The same signed index covers skills. See the gap below. |
| Ed25519 signed bundles | `graphrt-bundle` | A bundle can carry the skills an agent uses, pinned by the bundle manifest. |
| Knowledge bases | `graphrt-rag`, `/v1/knowledge` | Stay separate. Knowledge answers "what does the company know". Lessons answer "how should this agent behave". |
| Conversation memory (`memory.window`) | `graphrt-llm/src/nodes/common.rs` | Stays separate. It is history inside one conversation and not learning across runs. |
| Human feedback and scores | `POST /v1/runs/{id}/feedback`, `graphrt-cli/src/scores.rs` | The trusted human signal that learning starts from. |
| Owner only files, id validation, secret scrubbing | `graphrt_core::private_fs`, `redact` | Lesson stores are owner only, ids are validated before any path is built, and evidence is scrubbed. |
| Evaluation | `graphrt eval` | Measures whether a lesson or skill made an agent better before it is approved widely. |

Sibling repositories:

- **graphrt-plugins** publishes a catalog release with `index.json`, a slimmer
  `index-v2.json` and a minisign signature next to each.
- **graphrt-builder-desktop** is a Tauri app that links the Graphrt crates
  directly (`graphrt-core`, `graphrt-llm`, `graphrt-plugin`, `graphrt-mcp`,
  `graphrt-rag`, `graphrt-bundle`). It does not call `graphrt serve`. The
  contract that matters most for the desktop UI is therefore a **Rust API**.
  The HTTP API mirrors it for servers.

**Gap found.** The desktop app verifies the catalog signature. The `graphrt`
CLI does not: `graphrt plugin install <name>` checks each tarball against the
sha256 in an index it fetched over HTTPS and never checks who signed that
index. Skills installed from the catalog by the CLI will require a valid
signature from the start. Bringing plugin installs to the same level is a
separate change.

## 3. Threats and the answer to each

| # | Threat | Answer in the design |
| --- | --- | --- |
| T1 | **Malicious skill from a third party.** The package tells the agent to exfiltrate data, or ships code that does. | Skills cannot run code: Graphrt has no shell tool and never executes `scripts/`. A skill is text plus read only files. From the catalog it must come from a signed index. Every file is pinned by sha256 at install, and a file that changed later is refused. A scan reports findings in plain language before the install is confirmed. |
| T2 | **Prompt injection inside skill content.** The description or body carries instructions aimed at the model. | Only installed skills are listed. Names and descriptions are checked at install: length caps, no control, bidirectional or invisible characters, no markup tags. The body enters the conversation as a tool result inside a labelled wrapper, never as part of the operator's instructions. The capability limits of T1 and T5 still hold when an injection succeeds. |
| T3 | **Prompt injection through something learned.** Text that became a lesson steers later runs. | A lesson is injected only after approval. Lessons sit in a delimited section that tells the model they are notes which never override the operator's instructions or approval rules. Lesson text is capped and scanned on write. |
| T4 | **Memory poisoning.** The agent learns something false or harmful from untrusted input and carries it into other runs. | Every lesson records where it came from and how far that source is trusted. A lesson that was derived from run content (tool results, documents, web pages, model output) is marked untrusted and can never approve itself. Only a direct human signal can be approved automatically, and only when the owner enabled that and the lesson is low risk. Unapproved proposals expire. Everything is listed, reversible and removable by run or by user. |
| T5 | **A skill that quietly widens permissions.** It asks for more tools than it showed, or an update adds some. | A skill never grants itself anything. It lists the tools it wants. The owner grants a subset at install, and the grant is stored outside the package. At run time a tool reaches the agent through a skill only if it is declared, granted and present in the tool registry. Tools that arrive through a skill ask for approval before each call unless the grant says otherwise, and are offered to the model only after the skill was activated in that run. An update changes the content hash, so the skill stays off until the owner looks at what changed and grants again. |
| T6 | **Leaks between tenants, agents or users through what was learned.** | A lesson belongs to one scope: an agent, and optionally one user of that agent. A run reads the lessons of its agent and its own user and no others. A lesson that came from one user's run stays in that user's scope until the owner promotes it. Stores are per operator directory with owner only permissions. Evidence excerpts pass through the same secret scrubbing as run errors. |
| T7 | **Resource exhaustion.** Huge skills or unbounded lesson growth slow every run or fill the context. | Hard caps on file size, file count, body size, catalog size, lessons per scope and the characters of lessons injected per run. |

What is deliberately **not** claimed: the scan does not prove a skill is safe.
It is a heuristic and can be evaded. The guarantees above do not rest on it.

## 4. Decisions

### 4.1 Skills

**D1. Use `SKILL.md` as is. Read Agent Plugins packages for their skills.**
A directory that follows the Agent Skills specification installs unchanged. A
directory with a `plugin.json` installs each skill found under `skills/`.
`mcp.json` and `hooks.json` are reported and ignored for now, which the Agent
Plugins specification allows.
*Why:* the format is the de facto standard, and a skills only client conforms.
A private format would cut Graphrt users off from every skill already written.

**D2. Skills never execute code.** Files under `scripts/` are listed as present
and can be read as text, and nothing runs them. A skill that needs an action
uses a tool from a Graphrt plugin or an MCP server.
*Why:* Graphrt runs on phones and in 16 MiB containers where no shell exists.
Scripts are also where the measured risk is: every confirmed malicious skill in
the Snyk scan carried code, and script bundles were about twice as likely to be
vulnerable. Plugins already give Graphrt a place for code with declared
permissions, limits and checksums.

**D3. Activation goes through tools, lazily.** An agent that lists skills gets a
catalog of names and descriptions in its system prompt and two tools from a
`skills` provider: `load_skill(name)` returns the body with a list of its files,
and `read_skill_file(name, path)` returns one file. The `name` parameter is an
enum of the attached skills.
*Why:* this is the dedicated tool pattern from the client guide, it fits the
existing tool contract, and it keeps the cost of twenty skills at twenty short
lines until one is used.

**D4. A skill requests tools and the owner grants them.** `allowed-tools` holds
`provider/name` entries (or `provider/*`). Entries in another client's syntax,
such as `Bash(git:*)`, are shown as not supported. Grants are stored in the
skill index next to the installed copy, never in the package. Granted tools ask
for approval by default.
*Why:* this is the same request and grant model as plugin `permissions`. It
answers T5 and turns the experimental `allowed-tools` field into something with
one clear meaning in Graphrt.

**D5. Pin everything at install.** Installing copies the skill into a store
directory and writes an index entry: name, description, source, the sha256 of
every file, the grants and the scan findings. The catalog shown to models comes
from that index, so starting an agent reads one small file and no skill
directory. A file whose hash no longer matches is refused at the moment it
would be read.
*Why:* it is fast (no scan of directories at start), and it is the "pin by
content hash" control OWASP asks for. Verifying on read costs one hash of a few
kilobytes and removes the window between a check and a later use.

**D6. Skills are attached per agent.** The `agent` node gets an optional
`skills: [name, ...]` field. No field, no cost: the node does not look for the
service, adds no prompt text and registers no tool.
*Why:* modular and explicit. A business user ticks skills on an agent, which
maps to one list in the document.

**D7. Creating a skill from plain language is a draft, then an install.** The
runtime offers a function that turns a description or an example conversation
into a `SKILL.md` draft with a chat model, validates and scans it, and returns
it for review. Saving the reviewed draft is a normal install with source
`authored`.
*Why:* the business user never sees YAML, and a generated skill passes the same
gate as any other.

### 4.2 Self learning

**D8. The unit of learning is a lesson: one short, human readable statement
with provenance.** Fields: text (at most 500 characters), kind (preference,
fact, procedure or correction), scope (agent and optional user), status
(proposed, approved, rejected or retired), origin (which run, which signal,
who), trust, risk, counters and timestamps.
*Why:* this is the small curated memory that production systems converged on,
itemized so that one lesson can be corrected or removed without rewriting the
rest. A person can read it, which is the requirement for the business user
and the precondition for any human approval.

**D9. No change to model weights and no automatic rewrite of instructions.**
Learning adds approved lessons to a bounded section of the prompt. The
operator's instructions are never edited by the runtime.
*Why:* models come from API providers, so weights are out of reach. Automatic
prompt rewriting is the least proven approach and the one with the largest
blast radius.

**D10. Capture is cheap and on the request path. Thinking is not.** When a run
ends or feedback arrives, the host records a small observation. Turning
observations into lesson proposals, with or without a model, happens in a
separate step that the host schedules: a command, a background task of
`serve` when idle, or a call from the desktop app. A run only reads approved
lessons.
*Why:* this keeps the request path free of model calls, which is the split
LangMem and Hermes use, and it protects the cold start and idle memory numbers.

**D11. Trust follows the source, and approval follows trust and risk.**

| Origin of a lesson | Trust | Can be approved without a person |
| --- | --- | --- |
| Written by the owner | `owner` | Yes, it is the owner's own action |
| Direct feedback text from an authenticated user on their own run | `user` | Only if the owner turned on automatic approval, only for that user's own scope, and only when risk is low |
| Distilled by a model from run content | `derived` | Never |

Risk is high when a lesson touches tools, sending, payments, approvals,
credentials, links or addresses, or when the scan reports a finding. A high
risk lesson always waits for the owner.
*Why:* this is the answer to MINJA and MemoryGraft. Those attacks work because
text the agent read or produced flows back into memory as if it were trusted.
Here that path ends in a proposal that a person must approve.

**D12. Scope is the isolation boundary.** Lessons are stored per agent
namespace and optionally per user. Reading uses an exact match. Widening a
lesson from one user to the whole agent is an explicit action by the owner.
*Why:* it answers T6 with a rule that can be tested: a run for user A never
receives a lesson that is scoped to user B.

**D13. Everything learned can be seen, changed and removed.** List by agent,
user and status. Edit the text. Approve, reject, retire, delete. Forget
everything that came from one user or one run. Every change is appended to an
audit log.
*Why:* this is the view, correct and delete surface that production memory
features expose, plus the rollback OWASP asks for.

**D14. A repeated procedure becomes a skill draft.** When lessons of kind
`procedure` accumulate, the runtime can propose a skill built from them. It
goes through D7 and is installed only after approval.
*Why:* this is the skill library idea in its safe form. It connects the two
features: lessons are the short notes and skills are the longer procedures.

### 4.3 Shape in the codebase

| Piece | Location | Notes |
| --- | --- | --- |
| Service traits | `graphrt-core` (`skill`, `learning`) | Small, no file system or runtime dependency, so core stays portable. |
| `graphrt-skill` | new crate | Parsing, validation, scan, store, index, the `skills` tool provider, drafts. |
| `graphrt-learn` | new crate | Lessons, scopes, file store, policy, capture and distillation. |
| `agent` node | `graphrt-llm` | Optional `skills` and `learning` fields. |
| CLI and HTTP | `graphrt-cli` | Cargo features `skills` and `learning`, **off by default until the work is complete**, so default builds do not change. The `agent` node code sits behind `graphrt-llm` features of the same names for the same reason. |

## 5. Contract for user interfaces

The same operations exist as a Rust API (for the desktop app, which links the
crates) and as HTTP routes of `graphrt serve` (for web builders). Field names
are stable across both. Every text a business user sees is plain language.

The Rust API comes first, with the `graphrt skill` and `graphrt learn` commands
printing the same JSON shapes. The HTTP routes and the draft operations come
with the last delivery step.

### 5.1 Skills

| Operation | Rust | HTTP | Returns |
| --- | --- | --- | --- |
| List installed skills | `SkillStore::list` | `GET /v1/skills` | Name, description, source, enabled, whether it needs attention |
| Look at one skill before or after install | `SkillStore::inspect`, `inspect_dir` | `GET /v1/skills/{name}` | Description, the tools it asks for and which are granted, files, findings in plain language, whether it contains scripts that will not run |
| Install from a directory or a catalog entry | `SkillStore::install` | `POST /v1/skills` | The installed entry. Fails with the findings when a blocking finding exists and was not accepted. |
| Change grants, enable or disable | `SkillStore::set_grants`, `set_enabled` | `PATCH /v1/skills/{name}` | The updated entry |
| Remove | `SkillStore::remove` | `DELETE /v1/skills/{name}` | Nothing |
| Draft from plain language or a conversation | `draft_skill` | `POST /v1/skills/drafts` | A draft: name, description, body and findings, not yet installed |

One click install in a UI is `inspect` (show the summary and the tool
requests), then `install` with the grants the user ticked.

### 5.2 Learning

| Operation | Rust | HTTP | Returns |
| --- | --- | --- | --- |
| List what an agent learned | `LessonStore::list` | `GET /v1/learning/lessons?agent=&user=&status=` | Lessons with text, kind, status, trust, risk and an origin sentence such as "From feedback by sari@example.com on run r_81, 3 March" |
| Approve, reject, retire or edit | `LessonStore::decide`, `edit` | `PATCH /v1/learning/lessons/{id}` | The updated lesson |
| Add a lesson by hand | `LessonStore::add` | `POST /v1/learning/lessons` | The new lesson, approved, trust `owner` |
| Delete one | `LessonStore::delete` | `DELETE /v1/learning/lessons/{id}` | Nothing |
| Forget a user or a run | `LessonStore::forget` | `POST /v1/learning/forget` | How many lessons were removed |
| Record a signal | `Learner::observe` | Implicit: run end and `POST /v1/runs/{id}/feedback` | Nothing |
| Turn signals into proposals | `Learner::distill` | `POST /v1/learning/distill` | The new proposals |

## 6. Performance budget

Cold start and idle memory are selling points, and `bench/budgets.toml` gates
them in CI. The rules for this work:

- No work at process start. Stores are opened on first use.
- An agent without `skills` and without `learning` takes the same path as
  today.
- The catalog comes from one index file. Skill directories are not walked at
  run time.
- No model call for learning on the request path.
- Default builds do not link the new crates until the features are complete.

Each implementation pull request reports binary size, idle RSS and time to
healthy for the `agent-minimal` and `agent-slim` builds before and after, as
measured by `scripts/budget-gate.py`.

## 7. Delivery

Small pull requests in this order:

1. This document.
2. Skills: `graphrt-skill`, the core trait, the `skills` field of the `agent`
   node, `graphrt skill` commands, tests for T1, T2, T5 and T7.
3. Self learning: `graphrt-learn`, the `learning` field, `graphrt learn`
   commands, tests for T3, T4, T6 and T7.
4. Authoring and distribution: drafts from plain language and conversations,
   lessons to skill drafts, catalog installs with a required signature, and the
   HTTP routes.

Out of scope here: the desktop UI (it lives in graphrt-builder-desktop and
builds on the contract in section 5), publishing skills in the live catalog,
and executing skill scripts in a sandbox.
