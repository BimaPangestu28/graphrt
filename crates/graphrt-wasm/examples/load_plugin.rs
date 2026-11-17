//! Load a plugin and print its node catalog as JSON.
//!
//! `cargo run -p graphrt-wasm --example load_plugin -- path/to/plugin.wasm`

#![allow(clippy::print_stdout)]

use graphrt_core::Registry;
use graphrt_wasm::{WasmLimits, WasmPlugin};

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: load_plugin <plugin.wasm>");
        std::process::exit(2);
    };
    match WasmPlugin::load_file(&path, WasmLimits::default()) {
        Ok(plugin) => {
            let mut registry = Registry::with_builtins();
            if let Err(e) = plugin.register(&mut registry) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            match serde_json::to_string_pretty(&plugin.descriptors()) {
                Ok(json) => println!("{json}"),
                Err(e) => eprintln!("{e}"),
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
