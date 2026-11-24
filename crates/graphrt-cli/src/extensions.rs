//! Plugin discovery and the node registry used by a command.
//!
//! Every command that builds graphs (run, serve, validate, inspect, nodes) loads
//! [`Extensions`] once: the built-in node kinds of this build plus every plugin
//! found in the plugin search paths. Plugins come in three shapes:
//!
//! - a directory with a `graphrt-plugin.{json,yaml}` manifest (`process`, `http`
//!   or `wasm` runtime);
//! - a bare `.wasm` module placed directly in a search path (self-describing);
//! - nothing else: other files are ignored.
//!
//! A plugin that fails to load is reported as a warning and skipped, so one broken
//! plugin in `~/.graphrt/plugins` never breaks unrelated workflows.

#[cfg(any(feature = "plugins", feature = "wasm"))]
use std::path::Path;
use std::path::PathBuf;

use graphrt_core::Registry;
use graphrt_core::tool::ToolRegistry;
use serde::Serialize;

use crate::error::CliError;
use crate::setup;

/// Where to look for plugins.
#[derive(Clone, Debug, Default)]
pub struct PluginOptions {
    /// Extra plugin directories (`--plugins`), searched first.
    pub dirs: Vec<PathBuf>,
    /// Skip `$GRAPHRT_PLUGIN_PATH` and `~/.graphrt/plugins` (`--no-default-plugins`).
    pub no_default: bool,
    /// Plugins the operator trusts (`--trust-plugin`, `$GRAPHRT_TRUSTED_PLUGINS`): only
    /// these may receive plaintext secrets or send host variables as bearer tokens.
    pub trusted: Vec<String>,
    /// Warm process pool of each process plugin (`serve --plugin-pool`).
    pub pool: PluginPool,
}

/// How `serve` keeps process plugins warm (`--plugin-pool`, `--plugin-idle`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PluginPool {
    /// Processes kept started per process plugin (0: one, started at its first node).
    pub size: usize,
    /// Stop a pool's processes after this long without a request.
    pub idle_timeout: Option<std::time::Duration>,
}

/// Comma-separated plugin names trusted in addition to `--trust-plugin`.
pub const TRUSTED_PLUGINS_ENV: &str = "GRAPHRT_TRUSTED_PLUGINS";

impl PluginOptions {
    /// Trusted plugin names from the options and the environment.
    pub fn trusted_names(&self) -> Vec<String> {
        let mut names = self.trusted.clone();
        if let Ok(list) = std::env::var(TRUSTED_PLUGINS_ENV) {
            names.extend(list.split(',').map(str::trim).filter(|n| !n.is_empty()).map(str::to_owned));
        }
        names
    }
    /// Search paths in priority order.
    pub fn search_paths(&self) -> Vec<PathBuf> {
        let mut paths = self.dirs.clone();
        if !self.no_default {
            paths.extend(default_plugin_paths());
        }
        paths
    }
}

/// `$GRAPHRT_PLUGIN_PATH` entries, then `~/.graphrt/plugins`.
pub fn default_plugin_paths() -> Vec<PathBuf> {
    #[cfg(any(feature = "plugins", feature = "wasm"))]
    {
        graphrt_plugin::default_plugin_paths()
    }
    #[cfg(not(any(feature = "plugins", feature = "wasm")))]
    {
        Vec::new()
    }
}

/// One discovered plugin, for listings.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PluginSummary {
    pub name: String,
    pub version: String,
    pub description: String,
    /// `process`, `http` or `wasm`.
    pub runtime: String,
    /// Manifest or module path.
    pub path: PathBuf,
    /// Node kinds registered by the plugin.
    pub nodes: Vec<String>,
    /// Tools exposed by the plugin (as provider `<name>`).
    pub tools: Vec<String>,
    /// Why the plugin was skipped, when it failed to load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Requested permissions that were not granted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Node kinds and tools available to one command.
#[derive(Debug, Default)]
pub struct Extensions {
    /// Built-in kinds plus plugin kinds.
    pub registry: Registry,
    /// Tools contributed by plugins (merged with MCP tools by [`setup::services`]).
    pub tools: ToolRegistry,
    /// Every plugin found, including ones that failed to load.
    pub plugins: Vec<PluginSummary>,
    /// Keeps plugin hosts (and their processes) alive for the command's lifetime.
    #[cfg(any(feature = "plugins", feature = "wasm"))]
    hosts: Vec<graphrt_plugin::PluginHost>,
}

impl Extensions {
    /// Built-in node kinds only.
    pub fn builtin() -> Self {
        Self { registry: setup::builtin_registry(), ..Self::default() }
    }

    /// Built-in kinds plus every plugin found in `options`' search paths.
    pub fn load(options: &PluginOptions) -> Result<Self, CliError> {
        #[allow(unused_mut)]
        let mut ext = Self::builtin();
        #[cfg(any(feature = "plugins", feature = "wasm"))]
        {
            let paths = options.search_paths();
            let trusted = options.trusted_names();
            for manifest in graphrt_plugin::discover(&paths) {
                ext.add_manifest(&manifest, &trusted, options.pool);
            }
            #[cfg(feature = "wasm")]
            for module in bare_wasm_modules(&paths) {
                ext.add_bare_wasm(&module);
            }
            for plugin in &ext.plugins {
                if let Some(error) = &plugin.error {
                    eprintln!("warning: plugin `{}` skipped: {error}", plugin.name);
                }
                for warning in &plugin.warnings {
                    eprintln!("warning: {warning}");
                }
            }
        }
        #[cfg(not(any(feature = "plugins", feature = "wasm")))]
        if !options.dirs.is_empty() {
            return Err(CliError::usage("`--plugins` requires a build with the `plugins` or `wasm` feature"));
        }
        Ok(ext)
    }

    /// Starts every loaded process / HTTP plugin now, each on its own task,
    /// instead of at its first node (see [`graphrt_plugin::PluginHost::prewarm`]).
    /// Needs a Tokio runtime.
    pub fn prewarm_plugins(&self) {
        #[cfg(any(feature = "plugins", feature = "wasm"))]
        for host in &self.hosts {
            graphrt_core::task::spawn(host.prewarm());
        }
    }

    /// Names of the loaded plugins that would serve a node without starting a
    /// process first (see [`graphrt_plugin::PluginHost::warm_plugins`]).
    pub fn warm_plugins(&self) -> Vec<String> {
        #[cfg(any(feature = "plugins", feature = "wasm"))]
        {
            self.hosts.iter().flat_map(graphrt_plugin::PluginHost::warm_plugins).collect()
        }
        #[cfg(not(any(feature = "plugins", feature = "wasm")))]
        {
            Vec::new()
        }
    }

    /// Successfully loaded plugins.
    pub fn loaded(&self) -> impl Iterator<Item = &PluginSummary> {
        self.plugins.iter().filter(|p| p.error.is_none())
    }

    #[cfg(any(feature = "plugins", feature = "wasm"))]
    fn add_manifest(&mut self, manifest: &Path, trusted: &[String], pool: PluginPool) {
        let mut summary = PluginSummary { path: manifest.to_path_buf(), ..PluginSummary::default() };
        // One host per plugin: a failing plugin never leaves another half-registered.
        #[allow(unused_mut)]
        let mut host = graphrt_plugin::PluginHost::new().with_trusted(trusted.iter().cloned());
        #[cfg(feature = "plugins")]
        {
            host = host.with_pool(graphrt_plugin::PoolConfig { size: pool.size, idle_timeout: pool.idle_timeout });
        }
        #[cfg(not(feature = "plugins"))]
        let _ = pool;
        #[cfg(feature = "wasm")]
        {
            host = host.with_loader(std::sync::Arc::new(crate::wasm_loader::WasmLoader::default()));
        }
        let info = match host.load(manifest) {
            Ok(info) => info,
            Err(e) => {
                summary.name = manifest
                    .parent()
                    .and_then(Path::file_name)
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                summary.error = Some(e.to_string());
                self.plugins.push(summary);
                return;
            }
        };
        summary.name.clone_from(&info.name);
        summary.version.clone_from(&info.version);
        summary.description.clone_from(&info.description);
        summary.runtime.clone_from(&info.runtime);
        summary.tools.clone_from(&info.tools);
        summary.warnings.clone_from(&info.warnings);
        // Register into copies so a failure leaves the command's registry untouched.
        let mut registry = self.registry.clone();
        let mut tools = self.tools.clone();
        match host.register(&mut registry, &mut tools) {
            Ok(()) => {
                let source = format!("plugin:{}", info.name);
                summary.nodes =
                    registry.descriptors().into_iter().filter(|d| d.source == source).map(|d| d.kind).collect();
                self.registry = registry;
                self.tools = tools;
                self.hosts.push(host);
            }
            Err(e) => summary.error = Some(e.to_string()),
        }
        self.plugins.push(summary);
    }

    #[cfg(feature = "wasm")]
    fn add_bare_wasm(&mut self, module: &Path) {
        let mut summary = PluginSummary {
            path: module.to_path_buf(),
            runtime: "wasm".into(),
            name: module.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            ..PluginSummary::default()
        };
        match graphrt_wasm::WasmPlugin::load_file(module, graphrt_wasm::WasmLimits::default()) {
            Ok(plugin) => {
                let manifest = plugin.manifest();
                summary.name.clone_from(&manifest.name);
                summary.version.clone_from(&manifest.version);
                summary.description.clone_from(&manifest.description);
                // `register` refuses kinds that already exist (built-ins or other plugins).
                match plugin.register(&mut self.registry) {
                    Ok(()) => summary.nodes = plugin.descriptors().into_iter().map(|d| d.kind).collect(),
                    Err(e) => summary.error = Some(e.to_string()),
                }
            }
            Err(e) => summary.error = Some(e.to_string()),
        }
        self.plugins.push(summary);
    }
}

/// `.wasm` files directly inside each search path (or a path that is a `.wasm` file).
#[cfg(feature = "wasm")]
fn bare_wasm_modules(paths: &[PathBuf]) -> Vec<PathBuf> {
    let is_wasm = |p: &Path| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("wasm");
    let mut found = Vec::new();
    for path in paths {
        if is_wasm(path) {
            found.push(path.clone());
            continue;
        }
        let Ok(entries) = std::fs::read_dir(path) else { continue };
        let mut modules: Vec<PathBuf> =
            entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| is_wasm(p)).collect();
        modules.sort_unstable();
        found.extend(modules);
    }
    found
}

/// Add a hint to "unknown node type" errors pointing at plugin tooling.
pub fn explain_unknown_kind(message: String) -> String {
    if message.contains("unknown node type") {
        format!(
            "{message}\n  hint: install the plugin providing it (`graphrt plugin install <dir|git-url>`), \
             point `--plugins <dir>` at it, or check `graphrt nodes` for available kinds"
        )
    } else {
        message
    }
}
