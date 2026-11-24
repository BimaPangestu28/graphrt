//! # graphrt-wasm
//!
//! Sandboxed WebAssembly plugins that add node kinds to a graphrt
//! [`Registry`](graphrt_core::Registry).
//!
//! - **Portable:** one `.wasm` file runs on every OS and CPU the host runs on.
//! - **Sandboxed:** guests run in the pure-Rust wasmi interpreter with a fuel
//!   (CPU) budget, a memory cap, bounded I/O buffers and no system access. The
//!   only imports are `graphrt::log`, `graphrt::chunk` and `graphrt::progress`,
//!   plus `graphrt::http` for plugins the embedder grants an [`HttpEgress`].
//! - **Tools:** a module may also offer tools to `agent` nodes
//!   ([`WasmPlugin::tools`]), with the per-call tool context for trusted plugins.
//! - **Stateless:** every run gets a fresh instance.
//! - **Small:** wasmi adds well under a megabyte to a release binary.
//!
//! ```no_run
//! use graphrt_core::Registry;
//! use graphrt_wasm::{WasmLimits, WasmPlugin};
//!
//! # fn main() -> Result<(), graphrt_wasm::WasmError> {
//! let plugin = WasmPlugin::load_file("plugins/text_tools.wasm", WasmLimits::default())?;
//! let mut registry = Registry::with_builtins();
//! plugin.register(&mut registry)?; // adds e.g. `slugify` and `word-count`
//! # Ok(()) }
//! ```
//!
//! The guest ABI is documented in [`abi`] and in `docs/wasm-plugins.md`.

pub mod abi;
mod error;
mod host;
mod limits;
pub mod net;
mod plugin;
pub mod resolve;
mod tools;

pub use error::WasmError;
pub use host::LogLine;
pub use limits::WasmLimits;
pub use net::{HttpEgress, HttpError, HttpRequest, HttpResponse, WasmHost};
pub use plugin::WasmPlugin;
