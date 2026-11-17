# Contributing

Thanks for helping improve graphrt.

## Setup

Install a stable Rust toolchain (MSRV 1.85 for `graphrt-core`). Then:

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
