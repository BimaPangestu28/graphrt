# Plugin manifest specification

**Manifest format 1** (`protocol_version: 1`). JSON Schema:
[`schemas/plugin-manifest.schema.json`](schemas/plugin-manifest.schema.json).

This is the reference for `graphrt-plugin.json` / `graphrt-plugin.yaml`, written
from the parser in `crates/graphrt-plugin/src/manifest.rs`. For a tour of
plugins, the JSON-RPC protocol and the security model, see
[Plugins](plugins.md).

## Files

A plugin is a directory holding one manifest, looked up in this order:
`graphrt-plugin.json`, `graphrt-plugin.yaml`, `graphrt-plugin.yml`. Relative
paths in the manifest (`command` with a separator, `cwd`, `module`,
`checksums` keys) resolve against that directory.

JSON and YAML manifests follow the same rules: YAML is read into the same JSON
data model first, with YAML 1.2 boolean spellings only (`true` / `false`;
`yes`, `no`, `on` and `off` are strings).

## Strictness

- **Unknown keys are errors** in the manifest itself, `permissions`, `limits`,
  each `nodes[]` entry and each `outputs[]` entry, so a typo such as `comand`
  or `streming` fails at load time instead of being ignored. Tool entries and
  `handles` objects ignore unknown keys.
- Because of that, a JSON manifest **cannot carry a `$schema` key**. Map the
  schema in your editor instead (see [Editor setup](#editor-setup)).
- Fields typed `string | null` below accept `null` (same as omitting them).
  Plain `string`, `boolean`, `array` and `object` fields do not accept `null`.
- Integers must be non-negative whole numbers.
- A manifest is loaded in two steps: parsing (types, unknown keys, runtime
  fields), then [validation](#validation-rules) (names, kinds, limits, paths).
  Either step failing makes the plugin fail to load; `graphrt plugin list`
  shows the error and other plugins keep working.

## Top-level fields

| Field | Type | Default | Constraints |
| --- | --- | --- | --- |
| `name` | string | required | `[a-z0-9_-]{1,64}`. The plugin id: node kinds start with `<name>.`, tools are offered under provider `<name>`, and `graphrt plugin install` installs into `<dir>/<name>`. |
| `version` | string | `""` | Free-form for the host. The graphrt-plugins catalog requires semver (`MAJOR.MINOR.PATCH[-pre]`). |
| `description` | string | `""` | Shown by `graphrt plugin list` and `info`. |
| `author` | string \| null | `null` | |
| `license` | string \| null | `null` | SPDX expression recommended. |
| `homepage` | string \| null | `null` | |
| `protocol_version` | integer | `1` | Must equal the host's version, currently `1`. See [Compatibility](#compatibility-policy). |
| `runtime` | string | required | `process`, `http` or `wasm`. Selects which [runtime fields](#runtime-fields) apply. |
| `permissions` | object | all off | See [`permissions`](#permissions). |
| `limits` | object | defaults below | See [`limits`](#limits). |
| `checksums` | object | `{}` | See [`checksums`](#checksums). |
| `nodes` | array | `[]` | [Node descriptors](#nodes). |
| `tools` | array | `[]` | [Tools](#tools). |
| `allow_unprefixed_kinds` | boolean | `false` | See [Node kinds](#node-kinds). |

## Runtime fields

Runtime fields sit at the top level next to `runtime`. A field of another
runtime is an error when it is *present*: non-null for the optional strings,
non-empty for `args`, `env` and `headers`, `true` for `inherit_env`. (So
`"args": []` is accepted on an `http` plugin, `"args": ["x"]` is not.)

### `runtime: process`

A child process speaking newline-delimited JSON-RPC 2.0 over stdio, started on
first use and restarted after crashes.

| Field | Type | Default | Constraints |
| --- | --- | --- | --- |
| `command` | string | required | Not blank. A relative path with a separator (`./bin/plugin`, `bin/plugin`) is resolved against the plugin directory and must not be a symlink; a bare name (`python3`, `node`) is looked up in `PATH`; an absolute path (`/usr/bin/node`) is used as is. A relative path must not contain a `..` segment. |
| `args` | array of strings | `[]` | |
| `env` | object of strings | `{}` | Fixed environment variables. |
| `inherit_env` | boolean | `false` | `true` passes the whole host environment. Otherwise the process gets `PATH`, `HOME`, `LANG`, `TMPDIR` (and `SYSTEMROOT`, `USERPROFILE`, `APPDATA`, `TEMP`, `TMP`, `PATHEXT` on Windows) plus `permissions.env`. |
| `cwd` | string \| null | `null` | Working directory, a [relative path](#relative-paths) inside the plugin directory; default the plugin directory. |

### `runtime: http`

A service receiving JSON-RPC over `POST`.

| Field | Type | Default | Constraints |
| --- | --- | --- | --- |
| `url` | string | required | Starts with `http://` or `https://`. Must be `https://` (or a loopback address: `localhost`, `127.0.0.0/8`, `::1`) when `bearer_token_env` is set or `permissions.secrets` is `true`. |
| `headers` | object of strings | `{}` | Extra request headers. |
| `bearer_token_env` | string \| null | `null` | Host environment variable holding a bearer token. Must also be listed in `permissions.env`. The plugin must be trusted (`--trust-plugin`) or it does not load. |

### `runtime: wasm`

A WebAssembly module run by a loader registered for `wasm` (the graphrt CLI
ships one; see [WebAssembly plugins](wasm-plugins.md)).

| Field | Type | Default | Constraints |
| --- | --- | --- | --- |
| `module` | string | required | A [relative path](#relative-paths). |

## `permissions`

Requests, granted by the operator (see the [security model](plugins.md#security-model)).
Unknown keys are errors.

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `network` | boolean | `false` | Process and HTTP plugins: informational (they make network calls). WebAssembly plugins: grants the `graphrt::http` import through graphrt's egress policy ([wasm-plugins.md](wasm-plugins.md#outbound-http)); without it a module importing `http` does not load. |
| `env` | array of strings | `[]` | Host environment variables passed to a process plugin. Lists `bearer_token_env` for HTTP plugins. |
| `secrets` | boolean | `false` | Plaintext secrets in node config, resume input and `tool.call` `context` (an `agent` node's `tool_context`). Granted only to trusted plugins; others receive `******`. |

## `limits`

Unknown keys are errors. Omitted keys take their defaults.

| Field | Type | Default | Constraints |
| --- | --- | --- | --- |
| `timeout_ms` | integer | `60000` | At least 1. Per request; after 3 consecutive timeouts a process plugin is killed and restarted on the next request. |
| `max_concurrency` | integer | `16` | At least 1. Concurrent requests per plugin. |
| `max_message_bytes` | integer | `16777216` (16 MiB) | At least 1024. Largest protocol message; sent to the plugin in `initialize`. |

## `checksums`

An object mapping a [relative path](#relative-paths) to the file's sha256 in
hex (compared case-insensitively, surrounding whitespace ignored). Every entry
is verified by `graphrt plugin install` and before every process start; a
pinned path must be a regular file, not a symlink or directory.

When `checksums` is not empty it must pin the entrypoint: the wasm `module`, or
a process `command` that is a path inside the plugin directory (a leading `./`
is ignored when matching). A bare `command` such as `python3` has no pinned
entrypoint; pin the scripts it runs yourself. Checksums detect corruption and
partial edits; they are not signatures.

## Nodes

Each `nodes[]` entry is a node descriptor, the same shape as
`graphrt nodes --json` prints. Unknown keys are errors.

| Field | Type | Default | Constraints |
| --- | --- | --- | --- |
| `kind` | string | required | Node `type` in workflows. Not empty, no whitespace, unique in the manifest; see [Node kinds](#node-kinds). |
| `title` | string | required | |
| `description` | string | `""` | |
| `category` | string | `"other"` | `io`, `logic`, `transform`, `model`, `knowledge`, `integration`, `human` or `other`. |
| `config_schema` | any JSON | `{}` | JSON Schema of the node's config (the node fields other than `id`, `type`, `title`, `parent` and `policy`). Builders render config forms from it; `graphrt nodes` publishes it. |
| `outputs` | array | `[]` | [Outputs](#outputs). |
| `dynamic_outputs` | boolean | `false` | Outputs depend on config. |
| `handles` | object | `{"kind": "single"}` | [Handles](#handles). |
| `container` | boolean | `false` | Must be `false`: plugins cannot provide container nodes. |
| `icon` | string \| null | `null` | Cosmetic hint such as `globe`. |
| `source` | string | `""` | Ignored: the host sets `plugin:<name>`. |

### Node kinds

Every kind must start with `<name>.` (for plugin `acme`: `acme.sentiment`).
With `allow_unprefixed_kinds: true`, a kind may also be a plain name without
any `.` (`slugify`); a kind containing `.` must still start with `<name>.`, so
a plugin can never claim another plugin's namespace. Plugin kinds never
replace built-in kinds or another plugin's kinds.

### Outputs

Unknown keys are errors.

| Field | Type | Default | Constraints |
| --- | --- | --- | --- |
| `name` | string | required | |
| `type` | string | required | `any`, `none`, `string`, `secret`, `number`, `integer`, `float`, `boolean`, `object`, `file`, `array[any]`, `array[string]`, `array[number]`, `array[boolean]`, `array[object]`, `array[file]`. Aliases: `text-input`, `paragraph`, `select` (string), `checkbox` (boolean), `json` (object), `array` (array[any]), `file-list` (array[file]). |
| `description` | string | `""` | |
| `streaming` | boolean | `false` | Streamed incrementally (`node.chunk` notifications). |

### Handles

How a node picks its outgoing edges, tagged by `kind` (unknown keys ignored):

| `kind` | Other fields | Meaning |
| --- | --- | --- |
| `single` | | Every edge uses the default `source` handle. |
| `fixed` | `handles`: array of strings, required | A fixed set of handle ids. |
| `dynamic` | `config_path`: string \| null | Handles come from config; `config_path` points builders at the config field listing them. |

## Tools

Each `tools[]` entry is offered to `tool` and `agent` nodes under provider
`<name>`. Unknown keys are ignored.

| Field | Type | Default | Constraints |
| --- | --- | --- | --- |
| `name` | string | required | Not blank, unique in the manifest. |
| `description` | string | required | Shown to models. |
| `parameters` | any JSON | required | JSON Schema of the arguments object. |

## Relative paths

`module`, `checksums` keys and `cwd` are paths inside the
plugin directory: not blank, not absolute, and without a `..` segment (`/` and
`\` both separate segments).

## Validation rules

After parsing, a manifest is rejected when:

1. `name` is not `[a-z0-9_-]{1,64}`;
2. `protocol_version` is not the host's version (`1`);
3. `limits.max_concurrency` is 0, `limits.timeout_ms` is 0, or
   `limits.max_message_bytes` is below 1024;
4. a node `kind` is empty or contains whitespace or control characters, breaks the
   [prefix rule](#node-kinds), is a `container`, or is a duplicate;
5. a tool `name` is blank, contains control characters or is a duplicate;
6. a process `command` is blank or a relative path with a `..` segment, or
   its `cwd` is not a [relative path](#relative-paths); an http `url` is not `http(s)://`, or breaks
   the https / `permissions.env` rules for credentials; a wasm `module` is
   blank or not a [relative path](#relative-paths);
7. a `checksums` key is not a relative path, or a non-empty `checksums` does
   not pin the entrypoint.

The JSON Schema encodes the per-field rules (types, unknown keys, runtime
fields, names, limits, paths, enums). Rules that relate several fields (the
`<name>.` prefix, uniqueness, the pinned entrypoint, https for credentials,
`bearer_token_env` in `permissions.env`) are only checked by graphrt.

## Editor setup

JSON manifests cannot name their schema inline (unknown keys are errors), so
map it in the editor. VS Code `settings.json`:

```json
{
  "json.schemas": [
    {
      "fileMatch": ["graphrt-plugin.json"],
      "url": "https://raw.githubusercontent.com/Graphrt/graphrt/main/docs/schemas/plugin-manifest.schema.json"
    }
  ],
  "yaml.schemas": {
    "https://raw.githubusercontent.com/Graphrt/graphrt/main/docs/schemas/plugin-manifest.schema.json": ["graphrt-plugin.yaml", "graphrt-plugin.yml"]
  }
}
```

YAML manifests can instead start with a comment, which the parser ignores:
`# yaml-language-server: $schema=https://raw.githubusercontent.com/Graphrt/graphrt/main/docs/schemas/plugin-manifest.schema.json`.

## Compatibility policy

`protocol_version` is a single number covering the manifest format and the
JSON-RPC protocol together: a change to either is versioned by it. A host
loads only manifests whose `protocol_version` it speaks, and refuses a plugin
whose `initialize` reply reports a different version.

This section describes goals, not guarantees, until graphrt 1.0; from 1.0 on
it becomes a guarantee.

**Not breaking** (no `protocol_version` bump; ships in a graphrt minor release
and is listed in the changelog with the first version that understands it):

- a new optional field whose default keeps today's behaviour;
- a new accepted value of an existing field (a runtime, node category, value
  type or handles kind);
- relaxing a constraint, so that more manifests are accepted;
- new optional protocol methods, notifications or `features` the plugin may
  ignore.

Because unknown keys are errors, a manifest that *uses* a new field fails on
older hosts with a clear `unknown field` error. Plugin authors who need
older hosts should leave new fields out.

**Breaking** (requires `protocol_version` 2):

- removing or renaming a field, or changing its type or default;
- making an optional field required;
- tightening a constraint so that a manifest accepted today is rejected;
- changing what an existing field or protocol message means.

How a bump happens: the new version and its differences are documented here
and in a new schema file (`plugin-manifest-v2.schema.json`; this file then
stays frozen at version 1). The release that introduces version 2 is called
out under **Changed** in the changelog. Before 1.0, a `protocol_version` bump
aims to keep the previous version supported for at least two minor releases
(loaded next to the new one, with a deprecation warning); after 1.0 this
becomes a guarantee.

## Changes

| Format | graphrt | Changes |
| --- | --- | --- |
| 1 | 0.2.0 | First specified version. |
