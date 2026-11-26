//! The plugin host: loads manifests and registers plugin nodes and tools.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use graphrt_core::catalog::NodeDescriptor;
use graphrt_core::node::{NodeFactory, Registry};
use graphrt_core::tool::{ToolRegistry, ToolRuntime, ToolSpec};

use crate::discover::discover;
use crate::error::PluginError;
use crate::manifest::{LoadedManifest, Runtime, manifest_in};
use crate::node::{PluginShared, PluginTools, build_node};
use crate::transport::Transport;

/// What a [`PluginLoader`] produces for one plugin.
#[derive(Default)]
pub struct LoadedPlugin {
    /// Node kinds with their factories.
    pub nodes: Vec<NodeEntry>,
    /// Tools, registered under the plugin name.
    pub tools: Option<Arc<dyn ToolRuntime>>,
}

impl fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kinds: Vec<&str> = self.nodes.iter().map(|(d, _)| d.kind.as_str()).collect();
        f.debug_struct("LoadedPlugin").field("nodes", &kinds).field("tools", &self.tools.is_some()).finish()
    }
}

/// Executes plugins of a custom runtime (e.g. `wasm`).
pub trait PluginLoader: Send + Sync + fmt::Debug {
    /// Runtime name handled by this loader, as written in manifests.
    fn runtime(&self) -> &str;
    /// Load the plugin described by `plugin`.
    fn load(&self, plugin: &LoadedManifest) -> Result<LoadedPlugin, PluginError>;
}

/// Summary of a loaded plugin (for `plugin list` style output).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub runtime: String,
    pub dir: PathBuf,
    pub nodes: Vec<String>,
    pub tools: Vec<String>,
    /// Effective permissions (requested permissions the host did not grant are off).
    pub permissions: crate::manifest::Permissions,
    /// Whether the operator trusts this plugin (see [`PluginHost::trust`]).
    pub trusted: bool,
    /// Requested permissions that were not granted, and similar notes for operators.
    pub warnings: Vec<String>,
}

/// A node kind with its factory.
type NodeEntry = (NodeDescriptor, NodeFactory);

struct Entry {
    plugin: LoadedManifest,
    shared: Option<Arc<PluginShared>>,
    trusted: bool,
    warnings: Vec<String>,
}

/// Loads plugins and registers them into a [`Registry`] and [`ToolRegistry`].
///
/// Permissions in a manifest are *requests*. Plaintext secrets and HTTP bearer
/// tokens taken from the host environment are only granted to plugins the
/// operator trusts by name ([`PluginHost::trust`]); a manifest cannot grant them
/// to itself.
#[derive(Default)]
pub struct PluginHost {
    entries: Vec<Entry>,
    loaders: HashMap<String, Arc<dyn PluginLoader>>,
    trusted: std::collections::HashSet<String>,
}

impl fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.entries.iter().map(|e| e.plugin.manifest.name.as_str()).collect();
        let loaders: Vec<&String> = self.loaders.keys().collect();
        f.debug_struct("PluginHost").field("plugins", &names).field("loaders", &loaders).finish()
    }
}

impl PluginHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a loader for a custom runtime (e.g. `wasm`).
    pub fn with_loader(mut self, loader: Arc<dyn PluginLoader>) -> Self {
        self.loaders.insert(loader.runtime().to_owned(), loader);
        self
    }

    /// Trust the plugin named `name`: it may receive plaintext secrets and HTTP
    /// bearer tokens when its manifest requests them. Call before loading.
    pub fn trust(mut self, name: impl Into<String>) -> Self {
        self.trusted.insert(name.into());
        self
    }

    /// Trust several plugins by name (see [`PluginHost::trust`]).
    pub fn with_trusted<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.trusted.extend(names.into_iter().map(Into::into));
        self
    }

    /// Apply the host's grants to `plugin`'s requested permissions.
    fn apply_policy(&self, plugin: &mut LoadedManifest) -> Result<(bool, Vec<String>), PluginError> {
        let name = plugin.manifest.name.clone();
        let trusted = self.trusted.contains(&name);
        let mut warnings = Vec::new();
        if plugin.manifest.permissions.secrets && !trusted {
            plugin.manifest.permissions.secrets = false;
            warnings.push(format!(
                "plugin `{name}` requests plaintext secrets; they stay masked unless the plugin is trusted \
                 (e.g. `--trust-plugin {name}`)"
            ));
        }
        if let Runtime::Http(h) = &plugin.manifest.runtime {
            if let Some(var) = &h.bearer_token_env {
                if !trusted {
                    return Err(PluginError::Manifest(format!(
                        "plugin `{name}`: sending host variable `{var}` as a bearer token requires trusting the \
                         plugin (e.g. `--trust-plugin {name}`)"
                    )));
                }
            }
        }
        Ok((trusted, warnings))
    }

    /// Load the plugin whose manifest is at `manifest_path`.
    pub fn load(&mut self, manifest_path: &Path) -> Result<PluginInfo, PluginError> {
        let plugin = LoadedManifest::from_path(manifest_path)?;
        self.add(plugin)
    }

    /// Load a plugin directory, or every plugin directly inside `dir`.
    pub fn load_dir(&mut self, dir: &Path) -> Result<Vec<PluginInfo>, PluginError> {
        if let Some(manifest) = manifest_in(dir) {
            return self.load(&manifest).map(|i| vec![i]);
        }
        self.load_dirs(&[dir.to_path_buf()])
    }

    /// Load every plugin found under `paths` (see [`crate::discover`]).
    pub fn load_dirs(&mut self, paths: &[PathBuf]) -> Result<Vec<PluginInfo>, PluginError> {
        discover(paths).iter().map(|m| self.load(m)).collect()
    }

    /// Add an already-parsed plugin.
    pub fn add(&mut self, mut plugin: LoadedManifest) -> Result<PluginInfo, PluginError> {
        let name = plugin.manifest.name.clone();
        if self.entries.iter().any(|e| e.plugin.manifest.name == name) {
            return Err(PluginError::DuplicatePlugin(name));
        }
        let (trusted, warnings) = self.apply_policy(&mut plugin)?;
        let transport = make_transport(&plugin)?;
        let shared = transport.map(|t| Arc::new(PluginShared::new(&plugin.manifest, t)));
        let entry = Entry { plugin, shared, trusted, warnings };
        let summary = info(&entry);
        self.entries.push(entry);
        Ok(summary)
    }

    /// Add a plugin reachable over an existing transport (e.g. an in-process
    /// [`crate::IoTransport`]). `plugin.manifest.runtime` is ignored.
    pub fn add_with_transport(
        &mut self,
        mut plugin: LoadedManifest,
        transport: Arc<dyn Transport>,
    ) -> Result<PluginInfo, PluginError> {
        plugin.manifest.validate()?;
        let name = plugin.manifest.name.clone();
        if self.entries.iter().any(|e| e.plugin.manifest.name == name) {
            return Err(PluginError::DuplicatePlugin(name));
        }
        let (trusted, warnings) = self.apply_policy(&mut plugin)?;
        let shared = Some(Arc::new(PluginShared::new(&plugin.manifest, transport)));
        let entry = Entry { plugin, shared, trusted, warnings };
        let summary = info(&entry);
        self.entries.push(entry);
        Ok(summary)
    }

    /// Loaded plugins.
    pub fn plugins(&self) -> Vec<PluginInfo> {
        self.entries.iter().map(info).collect()
    }

    /// Descriptors of every plugin node (for catalogs).
    pub fn descriptors(&self) -> Vec<NodeDescriptor> {
        self.entries.iter().flat_map(|e| e.plugin.manifest.nodes.iter().map(|d| described(d, &e.plugin))).collect()
    }

    /// Start every process/HTTP plugin and apply node and tool lists returned by
    /// `initialize` (when a plugin returns them). Call before [`PluginHost::register`].
    pub async fn refresh(&mut self) -> Result<(), PluginError> {
        let clock = graphrt_core::node::default_clock();
        for entry in &mut self.entries {
            let Some(shared) = entry.shared.clone() else { continue };
            let name = entry.plugin.manifest.name.clone();
            let init = shared
                .transport
                .initialize(&clock)
                .await
                .map_err(|e| PluginError::Runtime { plugin: name.clone(), message: e.to_string() })?;
            let mut manifest = entry.plugin.manifest.clone();
            if let Some(nodes) = init.get("nodes").filter(|n| !n.is_null()) {
                manifest.nodes = serde_json::from_value(nodes.clone()).map_err(|e| {
                    PluginError::Manifest(format!("plugin `{name}`: invalid nodes from initialize: {e}"))
                })?;
            }
            if let Some(tools) = init.get("tools").filter(|t| !t.is_null()) {
                manifest.tools = serde_json::from_value::<Vec<ToolSpec>>(tools.clone()).map_err(|e| {
                    PluginError::Manifest(format!("plugin `{name}`: invalid tools from initialize: {e}"))
                })?;
            }
            manifest.validate()?;
            entry.plugin.manifest = manifest;
            entry.shared = Some(Arc::new(PluginShared::new(&entry.plugin.manifest, shared.transport.clone())));
        }
        Ok(())
    }

    /// Register every plugin's nodes into `registry` and tools into `tools`.
    /// Fails on node kinds that are already registered (plugins never replace
    /// built-in or other plugins' nodes).
    pub fn register(&self, registry: &mut Registry, tools: &mut ToolRegistry) -> Result<(), PluginError> {
        for entry in &self.entries {
            let name = &entry.plugin.manifest.name;
            let (nodes, tool_runtime): (Vec<NodeEntry>, Option<Arc<dyn ToolRuntime>>) = match &entry.shared {
                Some(shared) => {
                    let nodes = entry
                        .plugin
                        .manifest
                        .nodes
                        .iter()
                        .map(|d| {
                            let descriptor = described(d, &entry.plugin);
                            let shared = shared.clone();
                            let for_factory = descriptor.clone();
                            let factory: NodeFactory = Arc::new(move |doc| build_node(&shared, &for_factory, doc));
                            (descriptor, factory)
                        })
                        .collect();
                    let has_tools = !shared.tools.is_empty();
                    let tools: Option<Arc<dyn ToolRuntime>> =
                        has_tools.then(|| Arc::new(PluginTools { shared: shared.clone() }) as Arc<dyn ToolRuntime>);
                    (nodes, tools)
                }
                // An MCP preset has no code: the host that installed it connects the
                // server through its MCP client, so there is nothing to register here.
                None if matches!(entry.plugin.manifest.runtime, Runtime::Mcp(_)) => continue,
                None => {
                    let runtime = entry.plugin.manifest.runtime.name();
                    let loader = self
                        .loaders
                        .get(runtime)
                        .ok_or_else(|| PluginError::NoLoader { plugin: name.clone(), runtime: runtime.to_owned() })?;
                    let loaded = loader.load(&entry.plugin)?;
                    check_loader_kinds(&entry.plugin.manifest, &loaded.nodes)?;
                    let nodes = loaded.nodes.into_iter().map(|(d, f)| (described(&d, &entry.plugin), f)).collect();
                    (nodes, loaded.tools)
                }
            };
            for (descriptor, _) in &nodes {
                if registry.contains(&descriptor.kind) {
                    return Err(PluginError::KindConflict { plugin: name.clone(), kind: descriptor.kind.clone() });
                }
            }
            for (descriptor, factory) in nodes {
                registry.register_described(descriptor, move |doc| factory(doc));
            }
            if let Some(runtime) = tool_runtime {
                if tools.get(name).is_some() {
                    return Err(PluginError::Runtime {
                        plugin: name.clone(),
                        message: format!("a tool provider named `{name}` is already registered"),
                    });
                }
                tools.add(name.clone(), runtime);
            }
        }
        Ok(())
    }
}

fn described(descriptor: &NodeDescriptor, plugin: &LoadedManifest) -> NodeDescriptor {
    descriptor.clone().source(format!("plugin:{}", plugin.manifest.name))
}

fn info(entry: &Entry) -> PluginInfo {
    let plugin = &entry.plugin;
    let m = &plugin.manifest;
    PluginInfo {
        name: m.name.clone(),
        version: m.version.clone(),
        description: m.description.clone(),
        runtime: m.runtime.name().to_owned(),
        dir: plugin.dir.clone(),
        nodes: m.nodes.iter().map(|n| n.kind.clone()).collect(),
        tools: m.tools.iter().map(|t| t.name.clone()).collect(),
        permissions: m.permissions.clone(),
        trusted: entry.trusted,
        warnings: entry.warnings.clone(),
    }
}

/// Node kinds produced by a loader get the same namespace rules as manifest kinds:
/// `<name>.<kind>`, or (with `allow_unprefixed_kinds`) a plain name without `.`.
fn check_loader_kinds(manifest: &crate::manifest::PluginManifest, nodes: &[NodeEntry]) -> Result<(), PluginError> {
    let prefix = format!("{}.", manifest.name);
    let mut seen = std::collections::HashSet::new();
    for (descriptor, _) in nodes {
        let kind = descriptor.kind.as_str();
        let plain = manifest.allow_unprefixed_kinds && !kind.contains('.') && !kind.is_empty();
        if !kind.starts_with(&prefix) && !plain {
            return Err(PluginError::Manifest(format!(
                "plugin `{}`: node kind `{kind}` must start with `{prefix}`",
                manifest.name
            )));
        }
        if kind.chars().any(char::is_whitespace) || !seen.insert(kind) {
            return Err(PluginError::Manifest(format!(
                "plugin `{}`: invalid or duplicate node kind `{kind}`",
                manifest.name
            )));
        }
    }
    Ok(())
}

/// Transport for built-in runtimes; `None` for runtimes handled by a loader.
fn make_transport(plugin: &LoadedManifest) -> Result<Option<Arc<dyn Transport>>, PluginError> {
    let name = plugin.manifest.name.clone();
    match &plugin.manifest.runtime {
        #[cfg(feature = "process")]
        Runtime::Process(p) => Ok(Some(Arc::new(crate::process::ProcessTransport::new(plugin.clone(), p.clone())))),
        #[cfg(not(feature = "process"))]
        Runtime::Process(_) => Err(PluginError::RuntimeDisabled { plugin: name, runtime: "process".into() }),
        #[cfg(feature = "http")]
        Runtime::Http(h) => crate::http::HttpTransport::new(plugin, h.clone())
            .map(|t| Some(Arc::new(t) as Arc<dyn Transport>))
            .map_err(|e| PluginError::Runtime { plugin: name, message: e.to_string() }),
        #[cfg(not(feature = "http"))]
        Runtime::Http(_) => Err(PluginError::RuntimeDisabled { plugin: name, runtime: "http".into() }),
        _ => Ok(None),
    }
}
