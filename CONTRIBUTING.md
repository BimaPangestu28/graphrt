# Contributing

Thanks for helping improve graphrt.

## Setup

Install a stable Rust toolchain (MSRV 1.89 for every crate). Then:

```sh
cargo test --workspace
```

## Before opening a pull request

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test -p graphrt-core --no-default-features
```

Or run every CI job at once (fmt, clippy, test, MSRV, core without default
features, wasm32 and the min-size build) against your committed `HEAD`:

```sh
scripts/ci-local.sh            # --skip <job> to leave one out, e.g. --skip size
```

Live tests against real model APIs are opt-in and skipped by default. To run
them, set `GRAPHRT_LIVE_PROVIDERS=1` and the provider's key (see
[docs/providers.md](docs/providers.md#live-provider-tests)):

```sh
GRAPHRT_LIVE_PROVIDERS=1 ANTHROPIC_API_KEY=... \
  cargo test -p graphrt-llm --features gemini,bedrock --test live_providers -- --nocapture anthropic::
```

## Provider conformance tests

The gateway's cloud providers (Kubernetes, Cloud Run, Container Apps, ECS,
Lambda, Fly) run the shared C1–C13 conformance suite of
`graphrt-provider-api` plus provider-specific checks on every PR, against
local fake servers that model each cloud API. They need no cloud account.
Passing them does not verify a provider on the real platform: the cloud
targets stay "preview" until they have been checked on real accounts, and each
provider page under `docs/` lists what that still needs. The commands, the
fake servers and the per-case results are in
[crates/graphrt-provider-api/CONFORMANCE.md](crates/graphrt-provider-api/CONFORMANCE.md).

Keep that wording out of `docs/` and the README: those pages are published on
the project site, which describes the cloud targets as preview only.

## Code guidelines

- No `unwrap()`, `expect()`, `panic!()` or unchecked indexing outside tests
  (enforced by clippy). Return typed errors instead.
- Prefer explicit, readable code over clever one-liners.
- Add tests for every behavior change; node behavior is tested by calling
  `Node::run` with `NodeContext::for_test`, scheduling behavior through the engine.
- Keep `graphrt-core` free of runtime, network and C dependencies so it stays
  portable (including wasm32).
- Document public items.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/):
`feat(core): ...`, `fix(http): ...`, `docs: ...`, `refactor(llm): ...`.

## Reporting security issues

Please report vulnerabilities privately through GitHub security advisories
rather than public issues.

## Dependency and license policy

```sh
cargo deny check          # advisories, licenses (deny.toml), duplicate versions, sources
```

New dependencies must use a license in the `deny.toml` allow-list. Keep
`graphrt-core` dependencies minimal.

## Changelog

A pull request with a user-visible change adds a fragment to `changelog.d/`
(`<slug>.<kind>.md`, kind one of `added`, `changed`, `deprecated`, `removed`,
`fixed`, `security`; Markdown bullets) instead of editing `CHANGELOG.md`, so
pull requests in parallel do not conflict. See `changelog.d/README.md`;
`scripts/changelog.py check` validates them.

## Releasing (maintainers)

1. Run `scripts/changelog.py release X.Y.Z`: it moves `Unreleased` and the
   fragments in `changelog.d/` into a new version section of `CHANGELOG.md`
   and deletes the fragments. Then bump `version` in the root `Cargo.toml`
   `[workspace.package]` and `[workspace.dependencies]`.
   The section becomes the GitHub Release body, so start it with a short summary
   paragraph (2 to 4 sentences: what is new and why it matters), then `### Added` /
   `### Changed` / `### Fixed` / `### Security` with short bullets. Check how it
   renders with `scripts/release-notes.sh vX.Y.Z`. The install block it appends
   lives in `.github/release-notes/install.md`.
2. `scripts/publish.sh` — dry run: verifies `graphrt-core` against crates.io and
   packages every other crate.
3. `scripts/test-install.sh` — checks `install.sh` against a fake local release.
4. Tag `vX.Y.Z` and push the tag. The release workflow runs the test suite on
   Linux, macOS and Windows (`.github/workflows/cross-platform.yml`), builds
   binaries for Linux (static musl, x86_64 and aarch64), macOS (x86_64 and
   aarch64) and Windows (x86_64 MSVC, static C runtime), runs each binary that
   the runner can execute (`scripts/smoke-release-binary.sh`), publishes the GitHub Release
   "Graphrt X.Y.Z" (body from `scripts/release-notes.sh`) with `SHA256SUMS`, pushes `ghcr.io/<owner>/graphrt`, and publishes crates when the
   repository variable `PUBLISH_CRATES=true` and secret `CARGO_REGISTRY_TOKEN` are set.
