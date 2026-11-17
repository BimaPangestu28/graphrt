//! Size baseline: the same program without loading wasm, for measuring the
//! interpreter's footprint (`cargo build -p graphrt-wasm --profile min-size --examples`).

#![allow(clippy::print_stdout)]

use graphrt_core::Registry;

fn main() {
    let registry = Registry::with_builtins();
    match serde_json::to_string_pretty(&registry.descriptors()) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("{e}"),
    }
}
