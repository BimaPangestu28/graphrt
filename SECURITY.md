# Security policy

graphrt runs workflows that call models, tools, HTTP endpoints, code sandboxes and
plugins, often with access to secrets. We take security reports seriously.

## Reporting a vulnerability

**Do not open a public issue.** Report privately through
[GitHub Security Advisories](https://github.com/BimaPangestu28/agentic-rust-runtime/security/advisories/new).

Please include:

- affected crate(s) and version (`graphrt version`),
- a minimal workflow document or code that reproduces the issue,
- the impact you see (secret disclosure, SSRF, sandbox escape, denial of service...).

We aim to acknowledge reports within 3 working days and to ship a fix or
mitigation for confirmed high-severity issues within 30 days. We credit
reporters in the release notes unless you prefer otherwise.

## Supported versions

Until 1.0, only the latest minor release receives security fixes.

| Version | Supported |
| --- | --- |
| 0.1.x | yes |

## Scope

In scope: anything that breaks the guarantees documented in
[docs/architecture.md](docs/architecture.md) and [docs/credentials.md](docs/credentials.md), for example:

- secrets (`env.*`, secret-typed values, provider keys) appearing in events, logs,
  snapshots, metadata or error messages;
- the HTTP egress policy (private networks, redirects, DNS rebinding) being bypassed;
- escaping the JavaScript or WebAssembly sandboxes, or exceeding their memory / time / fuel limits;
- plugins receiving secrets or environment variables they were not granted;
- path traversal in file resolvers, plugin installation or bundles;
- resource exhaustion from a single malicious document, file or model response.

Out of scope: the Python subprocess runner (documented as **not** a sandbox),
plugins you trust explicitly (`--trust-plugin`), and issues requiring a
compromised host.

## Hardening checklist for deployments

- Run `graphrt serve` behind TLS with `--api-key-env` set.
- Keep `--allow-private-network` and `--allow-python-subprocess` off unless needed.
- Never ship provider keys in mobile apps; see [docs/credentials.md](docs/credentials.md).
- Pin plugin checksums and only trust plugins you have reviewed.
