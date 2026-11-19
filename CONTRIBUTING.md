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

## Releasing (maintainers)

1. Update `CHANGELOG.md` (move `Unreleased` into a new version section) and bump
   `version` in the root `Cargo.toml` `[workspace.package]` and `[workspace.dependencies]`.
2. `scripts/publish.sh` — dry run: verifies `graphrt-core` against crates.io and
   packages every other crate.
3. `scripts/test-install.sh` — checks `install.sh` against a fake local release.
4. Tag `vX.Y.Z` and push the tag. The release workflow builds binaries for Linux
   (static musl), macOS and Windows, publishes the GitHub Release with
   `SHA256SUMS`, pushes `ghcr.io/<owner>/graphrt`, and publishes crates when the
   repository variable `PUBLISH_CRATES=true` and secret `CARGO_REGISTRY_TOKEN` are set.
