# graphrt WebAssembly plugin SDK

- [`rust/graphrt-wasm-guest`](rust/graphrt-wasm-guest): Rust guest SDK (`Plugin` trait + `export_plugin!`).
- [`examples/text-tools`](examples/text-tools): example plugin with `slugify`, a streaming `word-count`
  and a branching `length-router` node.

Build the example:

```sh
rustup target add wasm32-unknown-unknown
cd examples/text-tools
cargo build --release --target wasm32-unknown-unknown
# -> target/wasm32-unknown-unknown/release/text_tools.wasm
```

Load it from Rust with `graphrt_wasm::WasmPlugin::load_file`. The ABI, security model and limits are
documented in [`docs/wasm-plugins.md`](../../docs/wasm-plugins.md).

These crates are standalone (not part of the host workspace) so they can be copied into any project.
