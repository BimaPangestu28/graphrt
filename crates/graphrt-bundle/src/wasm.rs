//! Registering bundled WebAssembly plugins.

use graphrt_core::Registry;
use graphrt_wasm::{WasmLimits, WasmPlugin};

use crate::bundle::Bundle;
use crate::error::BundleError;

impl Bundle {
    /// Load every bundled WebAssembly plugin and register its node kinds in
    /// `registry`. Returns the registered kinds.
    ///
    /// Kinds must start with `<plugin name>.`, unless the plugin manifest sets
    /// `allow_unprefixed_kinds` (then only plain names without `.` are allowed).
    /// Kinds that already exist in `registry` are never replaced: a plugin whose
    /// kinds are all registered already (e.g. the same plugin installed on the host)
    /// is skipped, and a partial overlap is an error. Bundled plugins
    /// never receive plaintext secrets: that requires host-side trust.
    /// Non-WebAssembly plugins are ignored here; see [`Bundle::check`].
    pub fn register_wasm_plugins(
        &self,
        registry: &mut Registry,
        limits: &WasmLimits,
    ) -> Result<Vec<String>, BundleError> {
        let mut kinds = Vec::new();
        for plugin in self.wasm_plugins()? {
            let err = |message: String| BundleError::Plugin { plugin: plugin.name.clone(), message };
            let module = plugin.module.as_deref().ok_or_else(|| err("missing module".into()))?;
            let wasm =
                WasmPlugin::load(module, limits.clone().with_expose_secrets(false)).map_err(|e| err(e.to_string()))?;
            let allow_plain =
                plugin.manifest.get("allow_unprefixed_kinds").and_then(serde_json::Value::as_bool).unwrap_or(false);
            let prefix = format!("{}.", plugin.name);
            for descriptor in wasm.descriptors() {
                let plain = allow_plain && !descriptor.kind.contains('.');
                if !descriptor.kind.starts_with(&prefix) && !plain {
                    return Err(err(format!(
                        "node kind `{}` must start with `{prefix}` (or be a plain name with `allow_unprefixed_kinds`)",
                        descriptor.kind
                    )));
                }
            }
            let descriptors = wasm.descriptors();
            if !descriptors.is_empty() && descriptors.iter().all(|d| registry.contains(&d.kind)) {
                continue;
            }
            wasm.register(registry).map_err(|e| err(e.to_string()))?;
            kinds.extend(wasm.descriptors().into_iter().map(|d| d.kind));
        }
        Ok(kinds)
    }
}
