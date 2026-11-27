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
| `workflow.yaml` | The entry workflow (more workflows may be listed) |
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

## Skills in a bundle (preview)

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

`graphrt serve` does not read bundled skills yet: as with bundled providers and
knowledge, a served bundle uses the host's services, here the host's skill
store. See [skills.md](skills.md).
