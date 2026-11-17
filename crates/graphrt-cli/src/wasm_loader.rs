//! Adapter running `runtime: wasm` plugin manifests with graphrt-wasm.
//!
//! The adapter lives in the CLI so neither library crate depends on the other:
//! graphrt-plugin knows manifests and the [`PluginLoader`] seam, graphrt-wasm
//! knows WebAssembly.

use graphrt_plugin::{LoadedManifest, LoadedPlugin, PluginError, PluginLoader, Runtime, verify_checksums};
use graphrt_wasm::{WasmLimits, WasmPlugin};

/// Loads WebAssembly plugins declared by manifests.
#[derive(Clone, Debug, Default)]
pub struct WasmLoader {
    limits: WasmLimits,
}

impl WasmLoader {
    /// Loader applying `limits` to every module.
    pub fn new(limits: WasmLimits) -> Self {
        Self { limits }
    }
}

impl PluginLoader for WasmLoader {
    fn runtime(&self) -> &str {
        "wasm"
    }

    fn load(&self, plugin: &LoadedManifest) -> Result<LoadedPlugin, PluginError> {
        let manifest = &plugin.manifest;
        let name = manifest.name.clone();
        let Runtime::Wasm(runtime) = &manifest.runtime else {
            return Err(PluginError::Manifest(format!("plugin `{name}` is not a wasm plugin")));
        };
        verify_checksums(plugin)?;
        let limits = self.limits.clone().with_expose_secrets(manifest.permissions.secrets);
        let module = plugin.resolve(&runtime.module);
        let wasm = WasmPlugin::load_file(&module, limits)
            .map_err(|e| PluginError::Runtime { plugin: name.clone(), message: e.to_string() })?;

        let prefix = format!("{name}.");
        let mut nodes = Vec::new();
        for descriptor in wasm.descriptors() {
            if !manifest.allow_unprefixed_kinds && !descriptor.kind.starts_with(&prefix) {
                return Err(PluginError::Manifest(format!(
                    "plugin `{name}`: module node kind `{}` must start with `{prefix}` \
                     (or set `allow_unprefixed_kinds: true`)",
                    descriptor.kind
                )));
            }
            let factory = wasm
                .factory(&descriptor.kind)
                .map_err(|e| PluginError::Runtime { plugin: name.clone(), message: e.to_string() })?;
            nodes.push((descriptor, factory));
        }
        Ok(LoadedPlugin { nodes, tools: None })
    }
}
