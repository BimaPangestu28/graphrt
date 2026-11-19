//! Plugin manifests (`graphrt-plugin.json` / `graphrt-plugin.yaml`).

use std::path::{Path, PathBuf};

use graphrt_core::catalog::NodeDescriptor;
use graphrt_core::tool::ToolSpec;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::PluginError;

/// Protocol version spoken by this host.
pub const PROTOCOL_VERSION: u32 = 1;

/// Manifest file names, in lookup order.
pub const MANIFEST_FILES: &[&str] = &["graphrt-plugin.json", "graphrt-plugin.yaml", "graphrt-plugin.yml"];

/// Default per-request timeout.
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// Default concurrent requests per plugin.
pub const DEFAULT_MAX_CONCURRENCY: usize = 16;
/// Default maximum protocol message size.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Largest protocol message size a manifest may ask for: the host buffers a whole
/// message in memory, so the cap must not be lifted without bound.
pub const MAX_MESSAGE_BYTES_CEILING: usize = 256 * 1024 * 1024;

/// A parsed and validated plugin manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawManifest")]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default = "default_protocol")]
    pub protocol_version: u32,
    #[serde(flatten)]
    pub runtime: Runtime,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub limits: Limits,
    /// `relative path -> sha256 hex`, verified before each process start.
    #[serde(default)]
    pub checksums: IndexMap<String, String>,
    #[serde(default)]
    pub nodes: Vec<NodeDescriptor>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// Allow node kinds that do not start with `<name>.`.
    #[serde(default)]
    pub allow_unprefixed_kinds: bool,
}

fn default_protocol() -> u32 {
    PROTOCOL_VERSION
}

/// Wire form of a manifest: every runtime field at the top level, unknown keys rejected
/// (so typos such as `comand` fail instead of being ignored).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default = "default_protocol")]
    protocol_version: u32,
    runtime: String,
    // process
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: IndexMap<String, String>,
    #[serde(default)]
    inherit_env: bool,
    #[serde(default)]
    cwd: Option<String>,
    // http
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: IndexMap<String, String>,
    #[serde(default)]
    bearer_token_env: Option<String>,
    // wasm
    #[serde(default)]
    module: Option<String>,
    #[serde(default)]
    permissions: Permissions,
    #[serde(default)]
    limits: Limits,
    #[serde(default)]
    checksums: IndexMap<String, String>,
    #[serde(default)]
    nodes: Vec<NodeDescriptor>,
    #[serde(default)]
    tools: Vec<ToolSpec>,
    #[serde(default)]
    allow_unprefixed_kinds: bool,
}

impl TryFrom<RawManifest> for PluginManifest {
    type Error = String;

    fn try_from(raw: RawManifest) -> Result<Self, String> {
        let misplaced = |runtime: &str, fields: &[(&str, bool)]| -> Result<(), String> {
            match fields.iter().find(|(_, present)| *present) {
                Some((field, _)) => Err(format!("`{field}` is not valid for runtime `{runtime}`")),
                None => Ok(()),
            }
        };
        let process_fields = [
            ("command", raw.command.is_some()),
            ("args", !raw.args.is_empty()),
            ("env", !raw.env.is_empty()),
            ("inherit_env", raw.inherit_env),
            ("cwd", raw.cwd.is_some()),
        ];
        let http_fields = [
            ("url", raw.url.is_some()),
            ("headers", !raw.headers.is_empty()),
            ("bearer_token_env", raw.bearer_token_env.is_some()),
        ];
        let wasm_fields = [("module", raw.module.is_some())];
        let runtime = match raw.runtime.as_str() {
            "process" => {
                misplaced("process", &http_fields)?;
                misplaced("process", &wasm_fields)?;
                Runtime::Process(ProcessRuntime {
                    command: raw.command.ok_or("runtime `process` requires `command`")?,
                    args: raw.args,
                    env: raw.env,
                    inherit_env: raw.inherit_env,
                    cwd: raw.cwd,
                })
            }
            "http" => {
                misplaced("http", &process_fields)?;
                misplaced("http", &wasm_fields)?;
                Runtime::Http(HttpRuntime {
                    url: raw.url.ok_or("runtime `http` requires `url`")?,
                    headers: raw.headers,
                    bearer_token_env: raw.bearer_token_env,
                })
            }
            "wasm" => {
                misplaced("wasm", &process_fields)?;
                misplaced("wasm", &http_fields)?;
                Runtime::Wasm(WasmRuntime { module: raw.module.ok_or("runtime `wasm` requires `module`")? })
            }
            other => return Err(format!("unknown runtime `{other}` (expected process, http or wasm)")),
        };
        Ok(Self {
            name: raw.name,
            version: raw.version,
            description: raw.description,
            author: raw.author,
            license: raw.license,
            homepage: raw.homepage,
            protocol_version: raw.protocol_version,
            runtime,
            permissions: raw.permissions,
            limits: raw.limits,
            checksums: raw.checksums,
            nodes: raw.nodes,
            tools: raw.tools,
            allow_unprefixed_kinds: raw.allow_unprefixed_kinds,
        })
    }
}

/// How the plugin is executed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "runtime", rename_all = "lowercase")]
#[non_exhaustive]
pub enum Runtime {
    /// A child process speaking newline-delimited JSON-RPC over stdio.
    Process(ProcessRuntime),
    /// A remote service receiving JSON-RPC over HTTP POST.
    Http(HttpRuntime),
    /// A WebAssembly module, executed by a [`crate::PluginLoader`] registered for `wasm`.
    Wasm(WasmRuntime),
}

impl Runtime {
    /// Runtime name as written in manifests.
    pub fn name(&self) -> &'static str {
        match self {
            Runtime::Process(_) => "process",
            Runtime::Http(_) => "http",
            Runtime::Wasm(_) => "wasm",
        }
    }
}

/// `runtime: process` settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessRuntime {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Fixed environment variables for the process.
    #[serde(default)]
    pub env: IndexMap<String, String>,
    /// Inherit the full host environment (off by default).
    #[serde(default)]
    pub inherit_env: bool,
    /// Working directory relative to the plugin directory.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// `runtime: http` settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HttpRuntime {
    pub url: String,
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    /// Name of a host environment variable holding a bearer token.
    #[serde(default)]
    pub bearer_token_env: Option<String>,
}

/// `runtime: wasm` settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WasmRuntime {
    /// Module path relative to the plugin directory.
    pub module: String,
}

/// What the plugin may access.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    /// Informational: the plugin makes network calls.
    #[serde(default)]
    pub network: bool,
    /// Host environment variables passed to the plugin process.
    #[serde(default)]
    pub env: Vec<String>,
    /// Send secret values in plaintext (otherwise masked).
    #[serde(default)]
    pub secrets: bool,
}

/// Resource limits for plugin requests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_concurrency")]
    pub max_concurrency: usize,
    #[serde(default = "default_message_bytes")]
    pub max_message_bytes: usize,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}
fn default_concurrency() -> usize {
    DEFAULT_MAX_CONCURRENCY
}
fn default_message_bytes() -> usize {
    DEFAULT_MAX_MESSAGE_BYTES
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

impl PluginManifest {
    /// Parse a manifest from JSON text.
    pub fn from_json(text: &str) -> Result<Self, PluginError> {
        let manifest: Self = serde_json::from_str(text).map_err(|e| PluginError::Manifest(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Parse a manifest from YAML text.
    #[cfg(feature = "yaml")]
    pub fn from_yaml(text: &str) -> Result<Self, PluginError> {
        let options = serde_saphyr::options! { strict_booleans: true };
        let raw: serde_json::Value =
            serde_saphyr::from_str_with_options(text, options).map_err(|e| PluginError::Manifest(e.to_string()))?;
        let manifest: Self = serde_json::from_value(raw).map_err(|e| PluginError::Manifest(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Read a manifest file (JSON or YAML by extension).
    pub fn from_file(path: &Path) -> Result<Self, PluginError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| PluginError::Io { path: path.to_path_buf(), message: e.to_string() })?;
        let is_yaml = matches!(path.extension().and_then(|e| e.to_str()), Some("yaml" | "yml"));
        let parsed = if is_yaml { Self::parse_yaml(&text) } else { Self::from_json(&text) };
        parsed.map_err(|e| match e {
            PluginError::Manifest(m) => PluginError::Manifest(format!("{}: {m}", path.display())),
            other => other,
        })
    }

    #[cfg(feature = "yaml")]
    fn parse_yaml(text: &str) -> Result<Self, PluginError> {
        Self::from_yaml(text)
    }

    #[cfg(not(feature = "yaml"))]
    fn parse_yaml(_text: &str) -> Result<Self, PluginError> {
        Err(PluginError::Manifest("YAML manifests need the `yaml` feature".into()))
    }

    /// Check names, kinds and limits.
    pub fn validate(&self) -> Result<(), PluginError> {
        let bad = |m: String| Err(PluginError::Manifest(format!("plugin `{}`: {m}", self.name)));
        if !is_valid_name(&self.name) {
            return bad("name must be 1-64 characters of lowercase letters, digits, `-` or `_`".into());
        }
        if self.protocol_version != PROTOCOL_VERSION {
            return bad(format!(
                "protocol_version {} is not supported (this host speaks {PROTOCOL_VERSION})",
                self.protocol_version
            ));
        }
        if self.limits.max_concurrency == 0 {
            return bad("limits.max_concurrency must be at least 1".into());
        }
        if self.limits.timeout_ms == 0 {
            return bad("limits.timeout_ms must be positive".into());
        }
        if self.limits.max_message_bytes < 1024 {
            return bad("limits.max_message_bytes must be at least 1024".into());
        }
        if self.limits.max_message_bytes > MAX_MESSAGE_BYTES_CEILING {
            return bad(format!("limits.max_message_bytes must be at most {MAX_MESSAGE_BYTES_CEILING}"));
        }
        let prefix = format!("{}.", self.name);
        let mut seen = std::collections::HashSet::new();
        for node in &self.nodes {
            if node.kind.trim().is_empty() || node.kind.chars().any(char::is_whitespace) {
                return bad(format!("invalid node kind `{}`", node.kind));
            }
            if !node.kind.starts_with(&prefix) {
                if !self.allow_unprefixed_kinds {
                    return bad(format!("node kind `{}` must start with `{prefix}`", node.kind));
                }
                // Unprefixed kinds are plain names: they may never claim another
                // plugin's `<name>.` namespace.
                if node.kind.contains('.') {
                    return bad(format!(
                        "node kind `{}` uses another namespace; unprefixed kinds must not contain `.`",
                        node.kind
                    ));
                }
            }
            if node.container {
                return bad(format!("node kind `{}`: container nodes are not supported by plugins", node.kind));
            }
            if !seen.insert(node.kind.as_str()) {
                return bad(format!("duplicate node kind `{}`", node.kind));
            }
        }
        let mut tool_names = std::collections::HashSet::new();
        for tool in &self.tools {
            if tool.name.trim().is_empty() || !tool_names.insert(tool.name.as_str()) {
                return bad(format!("invalid or duplicate tool name `{}`", tool.name));
            }
        }
        match &self.runtime {
            Runtime::Process(p) if p.command.trim().is_empty() => bad("`command` must not be empty".into()),
            Runtime::Http(h) if !(h.url.starts_with("http://") || h.url.starts_with("https://")) => {
                bad(format!("`url` must be an http(s) URL, got `{}`", h.url))
            }
            Runtime::Http(h) => self.validate_http(h),
            Runtime::Wasm(w) if w.module.trim().is_empty() => bad("`module` must not be empty".into()),
            Runtime::Wasm(w) if !is_inside(&w.module) => {
                bad(format!("`module` `{}` must be relative and inside the plugin directory", w.module))
            }
            _ => Ok(()),
        }?;
        for path in self.checksums.keys() {
            if !is_inside(path) {
                return bad(format!("checksum path `{path}` must be relative and inside the plugin directory"));
            }
        }
        // When checksums are used, the code that actually runs must be pinned too.
        if !self.checksums.is_empty() {
            let entrypoint = match &self.runtime {
                Runtime::Process(p) if Path::new(&p.command).components().count() > 1 && is_inside(&p.command) => {
                    Some(p.command.trim_start_matches("./"))
                }
                Runtime::Wasm(w) => Some(w.module.trim_start_matches("./")),
                _ => None,
            };
            if let Some(entry) = entrypoint {
                let pinned = self.checksums.keys().any(|k| k.trim_start_matches("./") == entry);
                if !pinned {
                    return bad(format!("`checksums` must include the entrypoint `{entry}`"));
                }
            }
        }
        Ok(())
    }

    /// HTTP plugins receive data over the network, so credentials and secrets
    /// travel only over TLS (loopback excepted) and the bearer-token variable must
    /// be declared in `permissions.env` (shown to operators at install time).
    fn validate_http(&self, h: &HttpRuntime) -> Result<(), PluginError> {
        let bad = |m: String| Err(PluginError::Manifest(format!("plugin `{}`: {m}", self.name)));
        let sensitive = h.bearer_token_env.is_some() || self.permissions.secrets;
        if sensitive && !h.url.starts_with("https://") && !is_loopback_url(&h.url) {
            return bad(format!(
                "`url` `{}` must use https:// when `bearer_token_env` or `permissions.secrets` is set",
                h.url
            ));
        }
        if let Some(var) = &h.bearer_token_env {
            if !self.permissions.env.iter().any(|e| e == var) {
                return bad(format!("`bearer_token_env` `{var}` must also be listed in `permissions.env`"));
            }
        }
        Ok(())
    }
}

/// Relative path without `..` segments.
fn is_inside(path: &str) -> bool {
    !path.trim().is_empty() && !Path::new(path).is_absolute() && !path.split(['/', '\\']).any(|seg| seg == "..")
}

/// Whether an http(s) URL points at the local machine.
fn is_loopback_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(stripped) = host.strip_prefix('[') {
        stripped.split(']').next().unwrap_or_default()
    } else {
        host.split(':').next().unwrap_or_default()
    };
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Valid plugin names: `[a-z0-9_-]{1,64}`.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// A manifest together with the directory it was loaded from.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedManifest {
    pub manifest: PluginManifest,
    /// Plugin directory (relative paths in the manifest resolve against it).
    pub dir: PathBuf,
}

impl LoadedManifest {
    /// Find and load the manifest inside `dir`.
    pub fn from_dir(dir: &Path) -> Result<Self, PluginError> {
        let path = manifest_in(dir)
            .ok_or_else(|| PluginError::Manifest(format!("no plugin manifest found in {}", dir.display())))?;
        Self::from_path(&path)
    }

    /// Load a manifest file; the plugin directory is its parent.
    pub fn from_path(path: &Path) -> Result<Self, PluginError> {
        let manifest = PluginManifest::from_file(path)?;
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        Ok(Self { manifest, dir })
    }

    /// Resolve a manifest-relative path.
    pub fn resolve(&self, relative: &str) -> PathBuf {
        self.dir.join(relative)
    }
}

/// The manifest file inside `dir`, if any.
pub fn manifest_in(dir: &Path) -> Option<PathBuf> {
    MANIFEST_FILES.iter().map(|f| dir.join(f)).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> serde_json::Value {
        serde_json::json!({
            "name": "acme",
            "runtime": "process",
            "command": "python3",
            "args": ["plugin.py"],
            "nodes": [{"kind": "acme.x", "title": "X"}]
        })
    }

    #[test]
    fn parses_process_manifest() {
        let m = PluginManifest::from_json(&base().to_string()).unwrap();
        assert_eq!(m.runtime.name(), "process");
        assert_eq!(m.limits.max_concurrency, DEFAULT_MAX_CONCURRENCY);
        assert_eq!(m.nodes[0].kind, "acme.x");
    }

    #[test]
    fn message_size_limit_has_a_ceiling() {
        let mut v = base();
        v["limits"] = serde_json::json!({"max_message_bytes": u64::MAX});
        assert!(PluginManifest::from_json(&v.to_string()).is_err());
        v["limits"] = serde_json::json!({"max_message_bytes": MAX_MESSAGE_BYTES_CEILING});
        assert!(PluginManifest::from_json(&v.to_string()).is_ok());
    }

    #[test]
    fn rejects_unprefixed_kind_and_bad_name() {
        let mut v = base();
        v["nodes"][0]["kind"] = "other.x".into();
        assert!(PluginManifest::from_json(&v.to_string()).is_err());
        // `allow_unprefixed_kinds` allows plain names but never another plugin's namespace.
        v["allow_unprefixed_kinds"] = true.into();
        assert!(PluginManifest::from_json(&v.to_string()).is_err());
        v["nodes"][0]["kind"] = "plain".into();
        assert!(PluginManifest::from_json(&v.to_string()).is_ok());
        let mut v = base();
        v["name"] = "Acme Corp".into();
        assert!(PluginManifest::from_json(&v.to_string()).is_err());
    }

    #[test]
    fn http_credentials_need_tls_and_declared_env() {
        let http = |url: &str, extra: serde_json::Value| {
            let mut v = serde_json::json!({
                "name": "acme", "runtime": "http", "url": url,
                "nodes": [{"kind": "acme.x", "title": "X"}]
            });
            if let (Some(obj), Some(more)) = (v.as_object_mut(), extra.as_object()) {
                for (k, val) in more {
                    obj.insert(k.clone(), val.clone());
                }
            }
            PluginManifest::from_json(&v.to_string())
        };
        // Bearer env must be declared, and sent only over TLS (loopback excepted).
        let token = serde_json::json!({"bearer_token_env": "TOKEN", "permissions": {"env": ["TOKEN"]}});
        assert!(http("http://evil.example/rpc", token.clone()).is_err());
        assert!(http("https://plugins.example/rpc", token.clone()).is_ok());
        assert!(http("http://127.0.0.1:9000/rpc", token.clone()).is_ok());
        assert!(http("http://localhost/rpc", token).is_ok());
        let undeclared = serde_json::json!({"bearer_token_env": "AWS_SECRET_ACCESS_KEY"});
        assert!(http("https://plugins.example/rpc", undeclared).is_err());
        let secrets = serde_json::json!({"permissions": {"secrets": true}});
        assert!(http("http://evil.example/rpc", secrets).is_err());
        assert!(http("http://evil.example/rpc", serde_json::json!({})).is_ok());
    }

    #[test]
    fn wasm_module_and_entrypoint_checksums() {
        let mut v = serde_json::json!({
            "name": "acme", "runtime": "wasm", "module": "../escape.wasm",
            "nodes": [{"kind": "acme.x", "title": "X"}]
        });
        assert!(PluginManifest::from_json(&v.to_string()).is_err());
        v["module"] = "plugin.wasm".into();
        assert!(PluginManifest::from_json(&v.to_string()).is_ok());
        // Checksums that leave the entrypoint unpinned are rejected.
        v["checksums"] = serde_json::json!({"other.txt": "00"});
        assert!(PluginManifest::from_json(&v.to_string()).is_err());
        v["checksums"] = serde_json::json!({"plugin.wasm": "00"});
        assert!(PluginManifest::from_json(&v.to_string()).is_ok());
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback_url("http://localhost:8080/x"));
        assert!(is_loopback_url("http://[::1]:80/"));
        assert!(is_loopback_url("http://user@127.0.0.2/"));
        assert!(!is_loopback_url("http://localhost.evil.example/"));
        assert!(!is_loopback_url("http://10.0.0.1/"));
    }

    #[test]
    fn rejects_unknown_fields_and_bad_paths() {
        let mut v = base();
        v["comand"] = "x".into();
        assert!(PluginManifest::from_json(&v.to_string()).is_err());
        let mut v = base();
        v["checksums"] = serde_json::json!({"../evil": "00"});
        assert!(PluginManifest::from_json(&v.to_string()).is_err());
        let mut v = base();
        v["protocol_version"] = 99.into();
        assert!(PluginManifest::from_json(&v.to_string()).is_err());
    }

    #[test]
    fn parses_http_and_wasm() {
        let http = serde_json::json!({"name": "h", "runtime": "http", "url": "https://p.example.com/rpc"});
        assert!(matches!(PluginManifest::from_json(&http.to_string()).unwrap().runtime, Runtime::Http(_)));
        let bad = serde_json::json!({"name": "h", "runtime": "http", "url": "ftp://x"});
        assert!(PluginManifest::from_json(&bad.to_string()).is_err());
        let wasm = serde_json::json!({"name": "w", "runtime": "wasm", "module": "plugin.wasm"});
        assert!(matches!(PluginManifest::from_json(&wasm.to_string()).unwrap().runtime, Runtime::Wasm(_)));
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn parses_yaml() {
        let m = PluginManifest::from_yaml("name: acme\nruntime: process\ncommand: node\nargs: [p.mjs]\n").unwrap();
        assert_eq!(m.name, "acme");
    }
}
