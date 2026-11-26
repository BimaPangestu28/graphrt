# Skills

**Preview.** Skills are opt in: the `graphrt` binary needs the `skills` cargo
feature (`cargo install graphrt-cli --features skills`). Rust hosts add the
`graphrt-skill` crate and turn on the `skills` feature of `graphrt-llm`.
Default builds are unchanged: an agent that lists skills is refused there with
a configuration error, so it never runs without them. The format of the skill
store may still change before this leaves preview.

A skill gives an agent a new ability without rewriting the agent. It is a
directory with a `SKILL.md` file: a name, a description, instructions, and
optionally reference files and a list of tools the skill would like to use.

```text
refund-policy/
  SKILL.md              name, description and instructions
  references/table.md   read only when the instructions point at it
```

```markdown
---
name: refund-policy
description: Decides whether an order can be refunded and how. Use when a customer asks for money back.
allowed-tools: orders/lookup
---

# Refunds

1. Look the order up before answering.
2. Orders older than 30 days get store credit. See references/table.md.
```

The format is the [Agent Skills specification](https://agentskills.io/specification),
so skills written for other agent products install as they are. An
[Agent Plugins](https://agent-plugins.org) package (`plugin.json` plus
`skills/<name>/SKILL.md`) installs each of its skills.

## Using a skill

```sh
graphrt skill inspect ./refund-policy                  # what it does, what it asks for, what the scan found
graphrt skill install ./refund-policy --grant orders/lookup
```

```yaml
- id: support
  type: agent
  model: { provider: anthropic, name: claude-sonnet-4-5 }
  instruction: "You are the support agent of {{#env.COMPANY#}}."
  skills: [refund-policy, shipping-times]
```

What happens in a run:

1. The system prompt gets one line per skill (its name and description). Twenty
   skills cost twenty short lines.
2. When a task matches a skill, the model calls the `load_skill` tool and gets
   the instructions. Files are read one at a time with `read_skill_file`.
3. Tools granted to a skill are offered to the model only after that skill was
   loaded in the run.

An agent without `skills` behaves exactly as before.

## Writing a skill from plain language

Nobody has to write `SKILL.md` by hand. A model drafts it from a description,
or from a conversation that shows how the task is done:

```sh
graphrt skill draft --model openai/gpt-4.1 \
  --describe "When a customer asks for a refund, look the order up first. Orders older than 30 days get store credit."
graphrt skill draft --model openai/gpt-4.1 --conversation chat.json --tool orders/lookup
```

The result is a folder (`<name>/SKILL.md`) to read and edit. It becomes a skill
only through `graphrt skill install`, so a draft passes the same validation and
scan as any other skill, and gets no tool unless you grant it.

A conversation can contain text that someone planted, so it is handled as
untrusted material:

- It is passed to the model as a quoted block, with the instruction that it is
  not addressed to the model.
- A draft can only ask for tools you listed with `--tool`. Anything else the
  model names is dropped, and asking grants nothing.
- Nothing is installed until a person has seen the draft.

`--conversation` takes a JSON array of `{"role": ..., "content": ...}` objects.
The latest 60 turns and about 24,000 characters are used.

## What a skill can and cannot do

| A skill can | A skill cannot |
| --- | --- |
| Add instructions and reference text to a conversation, on demand | Run code. Files under `scripts/` are text like any other file and graphrt never executes them |
| Ask for tools in `allowed-tools` | Give itself a tool. Only the owner grants tools, and the grant is stored outside the skill |
| Use a granted tool after it was loaded in the run | Use a tool it did not ask for, or one it asked for that was not granted |
| | Change after it was installed. Every file is pinned by its SHA-256 |

### Tools

`allowed-tools` lists `provider/tool` or `provider/*` entries, the same names
an `agent` node uses in `tools`. Entries in another product's syntax, such as
`Bash(git:*)`, are shown as not supported and cannot be granted.

| Grant | Effect |
| --- | --- |
| none (the default) | The skill loads, the tool is never offered |
| `--grant orders/lookup` | Offered after the skill was loaded. Every call pauses the run for approval (pause kind `tool_approval`, the same as `require_approval`) |
| `--allow orders/lookup` | Offered after the skill was loaded. Calls run without asking |

A tool that the agent lists in its own `tools` is not affected by a skill: it
is available from the start and follows the agent's `require_approval`.

A granted tool that this host does not have (its plugin was removed, say) is
left out and reported as a `skill_tool_unavailable` progress event.

### Pinned content

Installing copies the skill into the store and records the SHA-256 of every
file. Each read checks the file against that record. If a file was changed,
replaced by a link, or added afterwards, it is not served: the model gets an
error and none of the changed text.

Updating a skill is an install with `--replace`. `graphrt skill inspect <dir>`
shows what changed first, including newly requested tools. Earlier grants are
not carried over to the new content.

### The scan

`inspect` and `install` scan every text file and report findings in plain
language:

| Finding | Level | Meaning |
| --- | --- | --- |
| `hidden_characters` | stop | Invisible or text reordering characters that can hide instructions |
| `download_and_run` | stop | A command that downloads something and runs it |
| `overrides_instructions`, `hides_from_user`, `skips_approval`, `touches_credentials` | warn | Wording that often appears in hostile skills |
| `external_links`, `scripts_not_run`, `unsupported_tools` | note | Web addresses mentioned, scripts present, tool requests graphrt cannot grant |

A "stop" finding refuses the install until `--accept-findings` is passed.

The scan is a set of pattern checks and can be evaded by rewording. It is there
so the common bad patterns are seen at install time. The limits in the table
above do not depend on it.

A name or description that contains invisible characters or markup tags is
refused outright, because those two fields are placed in the system prompt.

## `graphrt skill`

Every subcommand takes `--dir <DIR>` (the skill store) and `--json`.

| Command | Description |
| --- | --- |
| `graphrt skill list` | Installed skills: state, granted and requested tools, description. |
| `graphrt skill inspect <name\|dir>` | One skill, installed or not: description, tool requests and grants, files, findings, and the changes against the installed version. Writes nothing. |
| `graphrt skill install <dir> [--grant T]... [--allow T]... [--accept-findings] [--replace]` | Install a skill directory or every skill of an Agent Plugins package. |
| `graphrt skill draft --model P/N (--describe TEXT \| --conversation FILE) [--tool T]... [--out DIR] [--providers FILE]` | Write a new skill with a model. The folder is then installed with `install`. |
| `graphrt skill grant <name> [--grant T]... [--allow T]...` | Replace the grants of an installed skill. No flags: grant nothing. |
| `graphrt skill disable <name>` / `enable <name>` | Hide a skill from every agent, or show it again. |
| `graphrt skill remove <name>` | Delete an installed skill. |

The store is `--dir`, else `$GRAPHRT_SKILLS_DIR`, else `~/.graphrt/skills`.
`graphrt run` and `graphrt serve` read the same default. It is created owner
only (`0700` directories, `0600` files on Unix).

A skill that an agent lists but that is not installed fails the node
(`skill_not_found`), so a typo never runs an agent without the skill its author
expected. A disabled skill is left out silently.

## HTTP API

`graphrt serve` in a build with the `skills` feature serves the default skill
store. Every route needs the API key.

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| GET | `/v1/skills` | | `{skills: [...]}` |
| GET | `/v1/skills/{name}` | | The skill with its tool requests, grants, files and findings |
| POST | `/v1/skills/drafts` | `{name, description, body, requested_tools?}` | The draft, checked and scanned. Nothing is installed |
| POST | `/v1/skills` | `{draft: {...}, grants?, accept_findings?, replace?}` | `201` and the installed skill (origin `authored`) |
| PATCH | `/v1/skills/{name}` | `{enabled?, grants?}` | The updated skill |
| DELETE | `/v1/skills/{name}` | | `204` |

A skill is sent as text and passes the same validation, scan and grant rules
as one installed from a directory. `409` with the `findings` means the scan
found something that needs `accept_findings`. Installing a package from a
catalog and drafting with a model are not offered over HTTP.

## Limits

| Limit | Value |
| --- | --- |
| `name` | 1 to 64 of `a-z`, `0-9`, `-` |
| `description` | 1,024 characters |
| `SKILL.md` | 256 KiB |
| Files per skill | 256, each up to 1 MiB, 8 MiB in total, 5 directory levels |
| Skills per agent | 64 |
| Catalog in the system prompt | About 12,000 characters. Descriptions are shortened evenly beyond that and every skill stays listed |

Symbolic links, Windows device names and file names that differ only in letter
case are refused. A tool result is cut at the agent's `max_tool_output_chars`
(32,000 by default), so keep `SKILL.md` well below that and move detail into
reference files.

## Embedding

```rust
use std::sync::Arc;
use graphrt_skill::{Grant, InstallOptions, SkillSet, SkillStore};

let store = Arc::new(SkillStore::open(app_data_dir.join("skills")));   // no I/O yet

// Review, then install with the grants the user ticked.
let review = store.inspect_dir(&downloaded_dir)?;                      // entry, findings, blocked, changes
let options = InstallOptions { grants: vec![Grant::ask("orders/lookup")], ..InstallOptions::default() };
store.install(&downloaded_dir, options)?;

// Make the skills available to `agent` nodes (graphrt-llm with feature `skills`).
services.insert(SkillSet::new(Arc::clone(&store)).into_service());
```

| Call | Use |
| --- | --- |
| `SkillStore::list`, `get`, `inspect` | Show installed skills |
| `SkillStore::inspect_dir` | The review screen before an install or an update |
| `SkillStore::install`, `install_package` | Install with explicit grants and an origin (`OriginKind::Catalog`, `verified: true` when the host checked a signature) |
| `SkillStore::set_grants`, `set_enabled`, `remove` | Manage |
| `draft_skill(&model, &DraftRequest)` | A `SkillDraft` (name, description, body, requested tools, findings) from a description or a conversation. `model` is any `graphrt_core::text_model::TextModel`, for example `graphrt_llm::text_model::ChatTextModel` (feature `text-model`) |
| `SkillDraft::new` | Rebuild a draft after the user edited it. Same validation and scan |
| `SkillStore::install_draft` | Install a draft with origin `authored`. `SkillDraft::write_to` writes it as a folder instead |
| `SkillSet::into_service` | The `Skills` service `agent` nodes look up |

`SkillEntry`, `SkillDetails`, `Finding` and `SkillChanges` serialize to JSON
with stable field names, and the CLI prints the same shapes with `--json`.

A host that fetches skills from a catalog verifies the catalog's signature
itself and records the result in `SkillOrigin`. The `graphrt` CLI installs from
directories only for now.
