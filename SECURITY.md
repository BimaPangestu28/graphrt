# Security policy

graphrt runs workflows that call models, tools, HTTP endpoints, code sandboxes and
plugins, often with access to secrets. We take security reports seriously.

## Supported versions

Until 1.0, the two most recent minor releases receive security fixes.

| Version | Supported |
| --- | --- |
| 0.4.x | yes |
| 0.3.x | yes (security fixes only) |
| < 0.3 | no |

## Reporting a vulnerability

**Do not open a public issue, pull request or discussion.** Report privately
through [GitHub Security Advisories](https://github.com/BimaPangestu28/graphrt/security/advisories/new).

Please include:

- the affected crate(s) and version (`graphrt version`);
- a minimal workflow document, config or code that reproduces the issue;
- the impact you see (secret disclosure, SSRF, sandbox escape, denial of
  service, and so on) and the deployment it applies to (`graphrt serve`, an
  embedding, mobile).

What to expect:

- We acknowledge reports within 3 working days.
- We aim to ship a fix or mitigation for confirmed high-severity issues within
  30 days, and to coordinate the disclosure date with you.
- Reporters are credited in the release notes and advisory unless they prefer
  otherwise.

## Threat model

The details of each mechanism are in [docs/security.md](docs/security.md).

**Trusted:**

- the operator, meaning whoever runs `graphrt serve` or embeds the crates;
- the host and its process environment;
- provider, MCP, knowledge and providers-file configs;
- installed plugins (`--trust-plugin` grants them more);
- the remote code sandbox service the operator configures;
- holders of the server API key.

**Untrusted:**

- workflow documents and everything in them: code-node scripts, URLs,
  templates, limits, node configs;
- run inputs and resume inputs;
- uploaded files and documents;
- model output and tool results;
- HTTP responses;
- unsigned bundles, which have no more authority than a workflow file plus a
  providers file.

graphrt should hold these guarantees against untrusted input:

1. **Secrets.** `type: secret` values, re-supplied resume secrets and provider
   credentials must not appear in events, SSE, the run store, snapshots, logs,
   OTel spans, `Debug` output or error messages. This holds whether the secret
   is alone or inside a larger string.
2. **Egress.** A workflow cannot reach loopback, private, link-local,
   unique-local or cloud metadata addresses, in any IP notation, through DNS
   rebinding or through redirects, unless the operator allows private networks.
3. **Sandboxes.** JavaScript in `code` nodes and WebAssembly plugins cannot
   reach the file system, network, process environment, other runs' data or
   host objects, and cannot exceed their memory, stack, time or fuel limits.
4. **Availability.** A single malicious document, file, script, model response
   or request cannot crash the process (abort, stack overflow, panic across
   the API) or grow memory without bound.
5. **Server.** With an API key set, no `/v1/*` route is reachable without it.
   Identifiers taken from requests cannot traverse paths.
6. **Bundles.** When trusted keys are configured, a bundle that is unsigned,
   signed by another key or modified is refused. Extraction never writes
   outside its directory.

## Scope

**In scope:** anything that breaks a guarantee above, or the documented
behaviour in [docs/security.md](docs/security.md),
[docs/architecture.md](docs/architecture.md) and
[docs/credentials.md](docs/credentials.md). Examples:

- a secret in an event, an error message or a snapshot;
- an SSRF bypass;
- escaping QuickJS or wasmi, or exceeding their limits;
- a workflow that aborts `graphrt serve`;
- path traversal in file resolvers, plugin installation or bundles;
- plugins receiving secrets they were not granted;
- an auth bypass;
- a vulnerable dependency we ship with a reachable code path.

**Out of scope:**

- **The Python subprocess runner** (`--allow-python-subprocess`). It is
  documented as **not** a sandbox: code runs with the host user's privileges.
- **Process plugins, MCP servers and the remote sandbox service.** They are
  native code or services you chose to run. We still accept reports about the
  env and file hygiene graphrt promises for them.
- **Operator configuration choices**, such as `--allow-private-network`,
  serving without an API key on a public interface, unsigned bundles without
  `GRAPHRT_TRUSTED_KEYS`, or `env` entries not declared `type: secret`.
- **Secrets transformed by code that legitimately received them**, for example
  base64-encoded by a script, or echoed by an HTTP endpoint in another form.
- **Inference about secret values through workflow logic** (conditions,
  timing) by the workflow's own author.
- **API key holders.** Anyone with the server API key is fully trusted. That
  includes forging resume snapshots.
- **Model behaviour.** Prompt injection that makes a model call tools the
  workflow author gave it is a workflow design issue, unless graphrt itself
  widens what the tools can do.
- **Compromised hosts**, and denial of service that needs more traffic than
  the configured limits allow.

## Hardening checklist

See [docs/security.md](docs/security.md#hardening-checklist-for-deployers) for the
full list. In short:

- Run `graphrt serve` behind TLS with `GRAPHRT_API_KEY` set. Always set it when
  binding anything but loopback, including in Docker.
- Keep `--allow-private-network` and `--allow-python-subprocess` off unless
  you need them.
- Declare every key `type: secret`.
- Never ship provider keys in mobile apps; see [docs/credentials.md](docs/credentials.md).
- Pin plugin checksums and trust only plugins you have reviewed.
- Set `GRAPHRT_TRUSTED_KEYS` to require signed bundles.
- Bound load with `--max-active-runs`, `--max-runs`, `--store-ttl-days`,
  `--timeout-ms` and `--max-steps`.
