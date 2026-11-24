//! Adapter running `runtime: wasm` plugin manifests with graphrt-wasm.
//!
//! The adapter lives in the CLI so neither library crate depends on the other:
//! graphrt-plugin knows manifests and the [`PluginLoader`] seam, graphrt-wasm
//! knows WebAssembly.
//!
//! Host services follow the manifest's `permissions`, like a process plugin's:
//!
//! - `env`: the listed host variables are handed to the guest in every request;
//! - `network`: the guest may import `graphrt::http`, served by
//!   [`crate::wasm_egress::ClientEgress`] (builds with the `http` feature);
//! - `secrets` (and `--trust-plugin`): plaintext secrets in node configs and the
//!   agent's per-call `tool_context` reach the guest.

use graphrt_plugin::{LoadedManifest, LoadedPlugin, PluginError, PluginLoader, Runtime, verify_checksums};
use graphrt_wasm::{WasmHost, WasmLimits, WasmPlugin};

/// Loads WebAssembly plugins declared by manifests.
#[derive(Clone, Debug, Default)]
pub struct WasmLoader {
    limits: WasmLimits,
    allow_private_network: bool,
}

impl WasmLoader {
    /// Loader applying `limits` to every module.
    pub fn new(limits: WasmLimits) -> Self {
        Self { limits, allow_private_network: false }
    }

    /// Let plugins with `permissions.network` reach loopback, private and
    /// link-local addresses (`--allow-private-network`).
    pub fn with_private_network(mut self, allow: bool) -> Self {
        self.allow_private_network = allow;
        self
    }

    /// Host services granted by `manifest`'s permissions.
    fn host_for(&self, manifest: &graphrt_plugin::PluginManifest) -> Result<WasmHost, PluginError> {
        let env = manifest
            .permissions
            .env
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|value| (name.clone(), value)))
            .collect();
        let host = WasmHost::default().with_env(env);
        if !manifest.permissions.network {
            return Ok(host);
        }
        #[cfg(feature = "http")]
        {
            let egress = crate::wasm_egress::ClientEgress::new(self.allow_private_network)
                .map_err(|message| PluginError::Runtime { plugin: manifest.name.clone(), message })?;
            Ok(host.with_egress(std::sync::Arc::new(egress)))
        }
        #[cfg(not(feature = "http"))]
        {
            let _ = self.allow_private_network;
            Ok(host)
        }
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
        let host = self.host_for(manifest)?;
        let wasm = WasmPlugin::load_file_with_host(&module, limits, host)
            .map_err(|e| PluginError::Runtime { plugin: name.clone(), message: e.to_string() })?;

        let prefix = format!("{name}.");
        let mut nodes = Vec::new();
        for descriptor in wasm.descriptors() {
            // Plain names are allowed with `allow_unprefixed_kinds`, but a module can
            // never claim another plugin's `<name>.` namespace.
            let plain = manifest.allow_unprefixed_kinds && !descriptor.kind.contains('.');
            if !descriptor.kind.starts_with(&prefix) && !plain {
                return Err(PluginError::Manifest(format!(
                    "plugin `{name}`: module node kind `{}` must start with `{prefix}` \
                     (or be a plain name without `.` with `allow_unprefixed_kinds: true`)",
                    descriptor.kind
                )));
            }
            let factory = wasm
                .factory(&descriptor.kind)
                .map_err(|e| PluginError::Runtime { plugin: name.clone(), message: e.to_string() })?;
            nodes.push((descriptor, factory));
        }
        Ok(LoadedPlugin { nodes, tools: wasm.tools() })
    }
}
