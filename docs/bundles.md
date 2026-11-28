# Bundles (`.grt`)

A bundle is one portable file holding everything a workflow needs: build it
once (by hand or from a builder platform), then run it on a laptop, in a
container, behind `graphrt serve`, or inside a phone app.

```sh
graphrt bundle build support.yaml -o support.grt     # package
graphrt bundle inspect support.grt                    # what's inside, does it run here?
graphrt run support.grt --input question="..."        # run it directly
graphrt serve bundles/                                # serve every .grt (and .yaml) in a directory
```

## What goes in

| Path | Content |
| --- | --- |
| `bundle.json` | Manifest: name, version, entry workflow, requirements, SHA-256 of every file |
| `bundle.sig` | Optional Ed25519 signature over `bundle.json` |
| `workflow.yaml` | The entry workflow |
| `workflows/` | The workflows its agents start as `subagents` |
| `plugins/<name>/` | Plugins: WebAssembly plugins are portable; process / HTTP plugins are kept but flagged non-portable |
| `knowledge/` | Knowledge-base config, plus prebuilt memory-store files it references |
| `providers.json` | Model providers file — **never secrets** |
| `assets/` | Anything else (`--include dir`) |

`graphrt bundle build` reads the workflow, finds node kinds this build does not
provide, and automatically includes the WebAssembly plugins that provide them
(from `--plugins` and the usual plugin search paths). Process and HTTP plugins
are only included when passed explicitly with `--plugin <dir>`, and hosts that
cannot spawn processes (phones) refuse such bundles.

```sh
graphrt bundle build triage.yaml \
  --plugins ./plugins \                 # where to look for WebAssembly plugins
  --knowledge kb.yaml \                 # ships the config and its memory-store file
  --providers providers.yaml \          # api_key_env only; inline keys are rejected
  --include ./prompts \                 # copied under assets/prompts
  --bundle-version 1.2.0 \
  -o triage.grt
```

## Subagents and budgets

An agent that lists `subagents` starts other workflows as tools. A bundle
carries them: `graphrt bundle build` reads the workflows the entry's agents
list, and what those list in turn, from the entry's folder (or `--workflows
DIR`) by file name (`research` is `research.yaml`, `.yml` or `.json`), and
keeps each under `workflows/<file name>` with its skills and WebAssembly
plugins. A listed workflow that is not there stops the build, and so does a
subagent that names the workflow being bundled. `graphrt serve` and `graphrt
run` link them to the entry. They are not served on their own: a bundle adds
no endpoint besides its entry, its agents start only the workflows it
carries, and no other served workflow starts those. Each runs on the bundle's
services, with its own memory scope (its name).

The budgets of every run of the bundle, child runs included, go in the
manifest's `limits`:

```sh
graphrt bundle build lead.yaml --providers providers.yaml \
  --max-tokens-per-run 50000 --max-cost-per-run 0.50 -o lead.grt
```

```json
"limits": { "max_total_tokens": 50000, "max_cost_micros": 500000 }
```

Cost is in the price unit of the bundled providers file (`prices` per model,
see `providers.md`), in millionths, so `--max-cost-per-run` needs
`--providers`. A budget of 0 is refused. A host that sets its own
`--max-tokens-per-run` or `--max-cost-per-run` gets the lower of the two (a
host whose `--providers` replace the bundle's counts the bundle's cost budget
with its own prices, and says so at start). `graphrt bundle inspect` lists the
subagents and the budgets.

A bundle with `limits` is format 2: a runtime that does not know budgets
refuses it rather than run it without them. A bundle without `limits` stays
format 1, which every runtime reads. `limits` refuses fields it does not know,
so a new budget comes with a new format version.

## Requirements and compatibility

The manifest records what a host must provide:

- `node_kinds` — every node kind used (built-in or from plugins);
- `plugins` — bundled plugins, with runtime and portability;
- `providers` — model providers referenced (`model.provider`), configured by the host;
- `env` — declared environment variables (names only; values never travel);
- `graphrt` — the runtime version requirement (`>=major.minor`).

`graphrt bundle inspect` (or `Bundle::check` in Rust) reports whether this build
can run the bundle after registering its WebAssembly plugins.

## Integrity and signatures

Opening a bundle always:

- rejects unsafe archives: absolute paths, `..`, backslashes, symlinks,
  duplicate entries, too many entries, oversized files and suspicious
  compression ratios (zip bombs);
- checks every file against its SHA-256 and rejects files not listed in the manifest.

Signatures make bundles verifiable wherever they land:

```sh
graphrt bundle keygen --out release.key           # prints the public key
graphrt bundle sign triage.grt --key release.key  # or: bundle build --sign-key release.key
graphrt bundle verify triage.grt --trust-key <public-key-hex>
graphrt run triage.grt --trust-key <public-key-hex>
```

When at least one key is trusted (`--trust-key`, or `$GRAPHRT_TRUSTED_KEYS`,
comma-separated), unsigned bundles and bundles signed by other keys are refused.
The signature covers `bundle.json`, which lists the hash of every file, so it
covers the whole bundle.

## Secrets

Bundles never carry credentials:

- providers files, knowledge configs and plugin manifests with inline secrets
  (`api_key`, `token`, `password`, an `Authorization` header...) are rejected at
  build time — reference environment variables (`api_key_env`, `${VAR}`) instead;
- workflow `env` declarations carry names only;
- bundled WebAssembly plugins never receive plaintext secrets.

## Running bundles

- `graphrt run app.grt`: verifies the bundle, registers its WebAssembly plugins,
  and uses its providers file and knowledge config unless `--providers` /
  `--knowledge` are given (bundled configs are extracted to a private temporary
  directory). A plugin already installed on the host with the same node kinds is
  used as is; bundled plugins never replace existing kinds.
- `graphrt serve dir/`: serves every `.grt` file (named after its file stem) next
  to plain workflow files. Services (providers, knowledge, MCP) come from the
  server's own flags.
- Phones and other embedded hosts: `Runtime.loadBundle(bytes, trustedKeys)` in
  the Kotlin / Swift bindings (`load_bundle` in Rust). Bundles needing process or
  HTTP plugins are rejected there.

## Library API

```rust
use graphrt_bundle::{Bundle, BundleBuilder, OpenOptions};

let bytes = BundleBuilder::new("hello", "1.0.0")
    .workflow("workflow.yaml", std::fs::read_to_string("hello.yaml")?)?
    .plugin_dir(std::path::Path::new("plugins/text-tools"))?
    .build()?;

let bundle = Bundle::open(&bytes, &OpenOptions::default().trust_key("…hex…"))?;
let mut registry = graphrt_core::Registry::with_builtins();
bundle.register_wasm_plugins(&mut registry, &Default::default())?;
assert!(bundle.check(&registry).is_compatible());
let graph = graphrt_core::Graph::build(bundle.workflow(None)?, &registry)?;
```

## Format

A bundle is a zip archive (deflate) written deterministically: sorted entries,
fixed timestamps and permissions, so the same inputs produce the same bytes
(set `$SOURCE_DATE_EPOCH` to pin `created_at`). `format_version` is `1`; hosts
refuse bundles with a newer format.

## Skills in a bundle

In a build with the `skills` feature, `graphrt bundle build` packs every skill
the workflow's `agent` nodes list (`skills: [name]`), taken from the skill
store with the grants given there:

```text
skills/index.json            the skills, their grants, whether findings were accepted
skills/<name>/SKILL.md       the skill's files, exactly as installed
```

A listed skill that is not installed is an error at build time.

`graphrt run bundle.grt` installs the bundled skills into a new private store
and uses that store for the run, so the bundle behaves the same on a machine
that has other skills or none. Each bundled skill goes through the normal
install: validation, the scan, and the rule that a grant must name a tool the
skill asks for. A bundle that fails any of them does not run.

The grants travel with the bundle because they are its author's decision, like
the `tools` an agent lists in the same bundle. The manifest pins the index and
every skill file, so a signature covers them. Require signatures
(`GRAPHRT_TRUSTED_KEYS`) for bundles you did not build.

`graphrt serve` does the same for each bundle it serves, with the same install
gate. A bundle that fails it stops the server from starting. A served bundle
and the server's own skill store live side by side under these rules:

| Rule | Why |
| --- | --- |
| A bundle that brings skills runs on its private store and on no other | It behaves the same on every host, and it cannot pick up a skill, or a grant, that the server's owner gave to something else |
| An agent in such a bundle that lists a skill the bundle did not bring fails (`skill_not_found`) | Falling back to a server skill of that name would hand the bundle that skill's grants |
| The server's skills are not changed by serving a bundle | A bundle is not an install into the server |
| `/v1/skills` lists a bundle's skills under `bundled` and cannot change them | The bundle pins them. To change one, build the bundle again |
| A grant in a bundle must name a tool the skill asks for | The same rule as for every install |
| The grants of each bundled skill are printed when the server starts | They came with the bundle, so the operator sees them every time |
| A bundle without skills uses the server's store, like a plain workflow file | Nothing was brought, so there is nothing to isolate |

A grant in a bundle reaches no tool the bundle could not already use: its
workflow may list any tool of the host in an agent's `tools`. A bundled skill
is recorded with `origin.verified: true` only when the bundle is signed by a
trusted key. See [skills.md](skills.md).
