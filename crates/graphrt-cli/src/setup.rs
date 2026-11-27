//! Building the node registry, host services and run inputs shared by every command.

use std::path::Path;
use std::sync::Arc;

use graphrt_core::engine::Limits;
use graphrt_core::{Graph, Registry, Services, Value, Workflow};
use indexmap::IndexMap;

use crate::error::CliError;

/// Options that affect which host services are configured.
#[derive(Clone, Debug, Default)]
pub struct ServiceOptions {
    /// Extra variables (from `--env-file`), merged over the process environment.
    pub env_file: Vec<(String, String)>,
    /// Allow HTTP requests to private / loopback addresses.
    pub allow_private_network: bool,
    /// Path to an MCP servers file (`{"mcpServers": {...}}`).
    pub mcp_config: Option<std::path::PathBuf>,
    /// Path to a model providers file (YAML or JSON).
    pub providers: Option<std::path::PathBuf>,
    /// Register the local `python3` subprocess runner for python code nodes.
    pub allow_python_subprocess: bool,
    /// `provider/name` applied to every model-backed node.
    pub model_override: Option<String>,
    /// Knowledge base config file for knowledge-retrieval nodes.
    pub knowledge: Option<std::path::PathBuf>,
    /// Root directory for `local_file` references.
    pub files_dir: Option<std::path::PathBuf>,
    /// File workspace of the agents (`--workspace`).
    #[cfg(feature = "files")]
    pub workspace: Option<WorkspaceOptions>,
    /// Plugin search paths.
    pub plugins: crate::extensions::PluginOptions,
    /// Public keys (hex) whose bundle signatures are trusted (`--trust-key`).
    pub trust_keys: Vec<String>,
    /// Ask nodes to report content (rendered messages, tool results...) because
    /// OpenTelemetry content capture is on.
    pub capture_content: bool,
    /// Skill store for `agent` nodes that list skills, instead of the default
    /// one (set when a bundle brings its own skills). Builds with `skills`.
    pub skills_dir: Option<std::path::PathBuf>,
}

/// The workspace the agents use (`--workspace`), kept in the host services so
/// other parts of the host (the `/v1/files` routes of `serve`) share its quota.
#[cfg(feature = "files")]
#[derive(Clone, Debug)]
pub struct ServedWorkspace {
    /// The unscoped workspace.
    pub workspace: graphrt_files::Workspace,
    /// `--workspace-per-user`: every user has a directory of their own.
    pub per_user: bool,
}

/// `--workspace` and its modifiers.
#[cfg(feature = "files")]
#[derive(Clone, Debug, Default)]
pub struct WorkspaceOptions {
    /// Directory the agents' files live in.
    pub dir: std::path::PathBuf,
    /// `--workspace-read-only`.
    pub read_only: bool,
    /// `--workspace-per-user`.
    pub per_user: bool,
    /// `--image-tools`.
    #[cfg(feature = "image")]
    pub image_tools: bool,
    /// `--image-model`.
    #[cfg(feature = "image")]
    pub image_model: Option<String>,
    /// `--document-tools`.
    #[cfg(feature = "documents")]
    pub document_tools: bool,
}

/// Environment variable naming a default providers file.
pub const PROVIDERS_ENV: &str = "GRAPHRT_PROVIDERS";

/// Node kinds whose config carries a `model {provider, name}` object.
pub const MODEL_NODE_KINDS: &[&str] = &["llm", "question-classifier", "parameter-extractor", "agent"];

/// Parse a `--model provider/name` override. The name may itself contain `/`.
pub fn parse_model_override(raw: &str) -> Result<(String, String), CliError> {
    match raw.split_once('/') {
        Some((provider, name)) if !provider.trim().is_empty() && !name.trim().is_empty() => {
            Ok((provider.trim().to_owned(), name.trim().to_owned()))
        }
        _ => Err(CliError::usage(format!("`--model` expects `provider/name`, got `{raw}`"))),
    }
}

/// Point every model-backed node at `provider` / `name`, keeping other model settings
/// (completion params). Returns the number of nodes changed.
pub fn apply_model_override(workflow: &mut Workflow, provider: &str, name: &str) -> usize {
    let mut changed = 0;
    for node in &mut workflow.graph.nodes {
        if !MODEL_NODE_KINDS.contains(&node.kind.as_str()) {
            continue;
        }
        let model = node.config.entry("model").or_insert_with(|| serde_json::json!({}));
        if let serde_json::Value::Object(map) = model {
            map.insert("provider".into(), serde_json::Value::String(provider.to_owned()));
            map.insert("name".into(), serde_json::Value::String(name.to_owned()));
            changed += 1;
        }
    }
    changed
}

impl ServiceOptions {
    /// Process environment overlaid with the env file.
    pub fn all_vars(&self) -> Vec<(String, String)> {
        let mut vars: Vec<(String, String)> = std::env::vars().collect();
        for (k, v) in &self.env_file {
            vars.retain(|(name, _)| name != k);
            vars.push((k.clone(), v.clone()));
        }
        vars
    }
}

/// Registry with every built-in node kind compiled into this binary (no plugins;
/// see [`crate::extensions::Extensions`]).
pub fn builtin_registry() -> Registry {
    #[allow(unused_mut)]
    let mut registry = Registry::with_builtins();
    #[cfg(feature = "llm")]
    graphrt_llm::register(&mut registry);
    #[cfg(feature = "http")]
    graphrt_http::register(&mut registry);
    #[cfg(feature = "code")]
    graphrt_code::register(&mut registry);
    #[cfg(feature = "doc")]
    graphrt_doc::register(&mut registry);
    #[cfg(feature = "rag")]
    graphrt_rag::register(&mut registry);
    registry
}

/// Read a workflow document from disk.
pub fn read_workflow(path: &Path) -> Result<Workflow, CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::usage(format!("cannot read workflow `{}`: {e}", path.display())))?;
    Workflow::parse(&text).map_err(|e| CliError::invalid(format!("{}: {e}", path.display())))
}

/// Parse and compile a workflow against `registry`.
pub fn load_graph(path: &Path, registry: &Registry) -> Result<Arc<Graph>, CliError> {
    load_graph_with(path, None, registry)
}

/// Parse and compile a workflow, optionally overriding every node's model (`provider/name`).
///
/// Every problem in the document is reported at once, rendered with source
/// snippets; warnings are printed to stderr.
pub fn load_graph_with(path: &Path, model_override: Option<&str>, registry: &Registry) -> Result<Arc<Graph>, CliError> {
    let (text, validation) = crate::diagnostics::validate_file(path, registry)?;
    let file = path.display().to_string();
    let report = crate::diagnostics::render_text(&file, &text, &validation.diagnostics);
    let Some(graph) = validation.graph else {
        return Err(CliError::invalid_report(report));
    };
    if !report.is_empty() {
        eprint!("{report}");
    }
    let Some(raw) = model_override else { return Ok(Arc::new(graph)) };
    let (provider, name) = parse_model_override(raw)?;
    let mut workflow = graph.workflow().clone();
    apply_model_override(&mut workflow, &provider, &name);
    let graph = Graph::build(workflow, registry)
        .map_err(|e| CliError::invalid(crate::extensions::explain_unknown_kind(format!("{file}: {e}"))))?;
    Ok(Arc::new(graph))
}

/// Parse a YAML or JSON document into JSON (YAML 1.2 booleans).
pub fn parse_structured(text: &str, what: &str) -> Result<serde_json::Value, CliError> {
    if text.trim_start().starts_with(['{', '[']) {
        return serde_json::from_str(text).map_err(|e| CliError::usage(format!("invalid {what}: {e}")));
    }
    graphrt_core::yaml::to_value(text, true).map_err(|e| CliError::usage(format!("invalid {what}: {e}")))
}

/// Build the host services for this build's features. `plugin_tools` (from
/// [`crate::extensions::Extensions`]) are merged with MCP tools into one
/// [`graphrt_core::tool::ToolRegistry`].
pub async fn services(
    options: &ServiceOptions,
    plugin_tools: graphrt_core::tool::ToolRegistry,
) -> Result<Services, CliError> {
    #[allow(unused_mut)]
    let mut services = Services::new();
    #[cfg(feature = "llm")]
    {
        let vars = options.all_vars();
        let mut models = graphrt_llm::ModelRegistry::from_vars(&vars)
            .map_err(|e| CliError::usage(format!("model configuration: {e}")))?;
        // A `typesafe` entry in the providers file's `credentials` section wins over
        // TYPESAFE_API_KEY. `source: external` entries are rejected while loading:
        // the CLI has no host app to supply credentials at run time.
        let mut system_one = None;
        if let Some(path) = &options.providers {
            let text = std::fs::read_to_string(path)
                .map_err(|e| CliError::usage(format!("cannot read providers file `{}`: {e}", path.display())))?;
            let value = parse_structured(&text, &format!("providers file `{}`", path.display()))?;
            #[allow(unused_mut)]
            let mut config = graphrt_llm::providers::ProvidersConfig::from_value(value)
                .map_err(|e| CliError::usage(format!("providers file `{}`: {e}", path.display())))?;
            // Replay cassette directories are relative to the providers file.
            #[cfg(feature = "replay")]
            config.resolve_paths(path.parent().unwrap_or_else(|| Path::new("")));
            let loaded =
                config.load(&vars).map_err(|e| CliError::usage(format!("providers file `{}`: {e}", path.display())))?;
            models.merge(loaded.models);
            system_one = loaded.system_one;
        }
        services.insert(models);
        // TypeSafe System One (Jev) decision model, when TYPESAFE_API_KEY is set.
        let system_one = match system_one {
            Some(client) => Some(client),
            None => graphrt_llm::typesafe::SystemOneClient::from_vars(&vars)
                .map_err(|e| CliError::usage(format!("TypeSafe configuration: {e}")))?,
        };
        if let Some(client) = system_one {
            services.insert(Arc::new(client));
        }
    }
    #[cfg(not(feature = "llm"))]
    {
        if let Some(path) = &options.providers {
            return Err(CliError::usage(format!(
                "`--providers {}` requires a build with the `llm` feature",
                path.display()
            )));
        }
    }
    if options.allow_python_subprocess {
        #[cfg(feature = "python")]
        {
            let sandbox: Arc<dyn graphrt_code::RemoteSandbox> = Arc::new(graphrt_code::SubprocessPython::new());
            services.insert(sandbox);
        }
        #[cfg(not(feature = "python"))]
        {
            return Err(CliError::usage("`--allow-python-subprocess` requires a build with the `python` feature"));
        }
    }
    // Agent skills: the store is opened lazily, so this costs nothing until an
    // `agent` node lists `skills`.
    #[cfg(feature = "skills")]
    if let Some(root) = options.skills_dir.clone().or_else(graphrt_skill::SkillStore::default_root) {
        let store = Arc::new(graphrt_skill::SkillStore::open(root));
        services.insert(graphrt_skill::SkillSet::new(store).into_service());
    }
    // Self learning: the store is opened lazily, so this costs nothing until an
    // `agent` node has `learning`.
    #[cfg(feature = "learning")]
    if let Some(root) = graphrt_learn::LessonStore::default_root() {
        services.insert(Arc::new(graphrt_learn::LessonStore::open(root)).into_service());
    }
    #[allow(unused_mut)]
    let mut plugin_tools = plugin_tools;
    // File access: inline data URLs, the agents' workspace, `--files-dir`, then
    // policy-checked HTTP URLs.
    #[allow(unused_mut)]
    let mut files = graphrt_core::file::ChainResolver::with_data_urls();
    #[cfg(feature = "files")]
    if let Some(workspace) = &options.workspace {
        // Before `--files-dir`, which would look for a `workspace:` reference on disk.
        let (served, resolver) = install_workspace(workspace, &options.all_vars(), &mut plugin_tools)?;
        files = files.then(resolver);
        // `graphrt serve --workspace-api` serves the same workspace, so the quota counts once.
        services.insert(served);
    }
    if let Some(dir) = &options.files_dir {
        if !dir.is_dir() {
            return Err(CliError::usage(format!("`--files-dir {}` is not a directory", dir.display())));
        }
        files = files.then(Arc::new(crate::files::LocalDirResolver::new(dir)));
    }
    #[cfg(feature = "http")]
    {
        let policy = graphrt_http::EgressPolicy { allow_private: options.allow_private_network, ..Default::default() };
        let client = graphrt_http::HttpClient::new(policy).map_err(|e| CliError::usage(format!("http client: {e}")))?;
        files = files.then(Arc::new(graphrt_http::HttpFileResolver::new(client.clone())));
        services.insert(client);
    }
    let files: Arc<dyn graphrt_core::file::FileResolver> = Arc::new(files);
    // Every image a model node sends goes through the same check: real type,
    // bounded size and pixels, no metadata, downscaled when too large.
    #[cfg(feature = "image")]
    {
        let preparer: Arc<dyn graphrt_core::file::ImagePreparer> =
            Arc::new(graphrt_image::VisionPreparer::new(Arc::clone(&files), graphrt_image::ImageLimits::default()));
        services.insert(preparer);
    }
    services.insert(files);
    if let Some(path) = &options.knowledge {
        #[cfg(feature = "rag")]
        {
            services.insert(load_knowledge(path, &options.all_vars())?);
        }
        #[cfg(not(feature = "rag"))]
        {
            return Err(CliError::usage(format!(
                "`--knowledge {}` requires a build with the `rag` feature",
                path.display()
            )));
        }
    }
    #[cfg(feature = "mcp")]
    {
        let tools = match &options.mcp_config {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| CliError::usage(format!("cannot read MCP config `{}`: {e}", path.display())))?;
                let servers: graphrt_mcp::McpServers = serde_json::from_str(&text)
                    .map_err(|e| CliError::usage(format!("invalid MCP config `{}`: {e}", path.display())))?;
                graphrt_mcp::connect_all(&servers.into_configs())
                    .await
                    .map_err(|e| CliError::usage(format!("MCP: {e}")))?
            }
            None => graphrt_core::tool::ToolRegistry::new(),
        };
        services.insert(merge_tools(plugin_tools, &tools)?);
    }
    #[cfg(not(feature = "mcp"))]
    {
        if let Some(path) = &options.mcp_config {
            return Err(CliError::usage(format!("`--mcp {}` requires a build with the `mcp` feature", path.display())));
        }
        services.insert(plugin_tools);
    }
    if options.capture_content {
        services.insert(graphrt_core::node::CaptureContent);
    }
    Ok(services)
}

/// Open the `--workspace` directory, register the `files` tool provider and
/// return the resolver for the files agents store there.
#[cfg(feature = "files")]
fn install_workspace(
    options: &WorkspaceOptions,
    vars: &[(String, String)],
    tools: &mut graphrt_core::tool::ToolRegistry,
) -> Result<(ServedWorkspace, Arc<dyn graphrt_core::file::FileResolver>), CliError> {
    use graphrt_files::{FileTools, PROVIDER, Workspace, WorkspaceLimits, WorkspaceResolver};
    let limits = WorkspaceLimits { read_only: options.read_only, ..WorkspaceLimits::default() };
    let workspace = match options.dir.to_str().and_then(|dir| dir.strip_prefix(S3_SCHEME)) {
        Some(location) => object_storage_workspace(location, limits, vars)?,
        None => Workspace::dir(&options.dir, limits)
            .map_err(|e| CliError::usage(format!("`--workspace {}`: {e}", options.dir.display())))?,
    };
    if tools.get(PROVIDER).is_some() {
        return Err(CliError::usage(format!(
            "tool provider `{PROVIDER}` is defined by a plugin and by `--workspace`; rename the plugin's provider"
        )));
    }
    #[cfg(feature = "image")]
    install_image_tools(options, vars, &workspace, tools)?;
    #[cfg(not(feature = "image"))]
    let _ = vars;
    let served = ServedWorkspace { workspace: workspace.clone(), per_user: options.per_user };
    #[cfg(feature = "documents")]
    install_document_tools(options, &workspace, tools)?;
    let file_tools = FileTools::new(workspace.clone());
    let (file_tools, resolver) = if options.per_user {
        (file_tools.scoped_by_context(), WorkspaceResolver::across_scopes(workspace))
    } else {
        (file_tools, WorkspaceResolver::new(workspace))
    };
    tools.add(PROVIDER, Arc::new(file_tools));
    Ok((served, Arc::new(resolver)))
}

/// Prefix of a `--workspace` kept in object storage: `s3://bucket/prefix`.
#[cfg(feature = "files")]
const S3_SCHEME: &str = "s3://";

/// A workspace in S3 or an S3 compatible store (`--workspace s3://bucket/prefix`).
///
/// Region from `GRAPHRT_S3_REGION`, `AWS_REGION` or `AWS_DEFAULT_REGION`
/// (default `us-east-1`), credentials from the usual AWS variables and roles.
/// For other stores: `GRAPHRT_S3_ENDPOINT` (`https://host[:port]`),
/// `GRAPHRT_S3_PATH_STYLE=false` for bucket host names, and
/// `GRAPHRT_S3_ALLOW_HTTP=true` to accept an `http://` endpoint.
///
/// @throws CliError (usage) - An invalid location or setting, or a build without `files-s3`.
#[cfg(feature = "files")]
fn object_storage_workspace(
    location: &str,
    limits: graphrt_files::WorkspaceLimits,
    vars: &[(String, String)],
) -> Result<graphrt_files::Workspace, CliError> {
    #[cfg(feature = "files-s3")]
    {
        let lookup = |name: &str| {
            let found = vars.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value.clone());
            found.filter(|value| !value.is_empty())
        };
        let flag = |name: &str| match lookup(name).as_deref() {
            None => Ok(None),
            Some("true") => Ok(Some(true)),
            Some("false") => Ok(Some(false)),
            Some(other) => Err(CliError::usage(format!("`{name}` must be `true` or `false`, not `{other}`"))),
        };
        let (bucket, prefix) = location.split_once('/').unwrap_or((location, ""));
        let region = ["GRAPHRT_S3_REGION", "AWS_REGION", "AWS_DEFAULT_REGION"].into_iter().find_map(lookup);
        let config = graphrt_files::S3Config {
            endpoint: lookup("GRAPHRT_S3_ENDPOINT"),
            prefix: Some(prefix.to_owned()).filter(|prefix| !prefix.is_empty()),
            path_style: flag("GRAPHRT_S3_PATH_STYLE")?,
            allow_http: flag("GRAPHRT_S3_ALLOW_HTTP")?.unwrap_or(false),
            ..graphrt_files::S3Config::new(bucket, region.unwrap_or_else(|| "us-east-1".to_owned()))
        };
        let store = graphrt_files::S3Store::from_lookup(&config, lookup)
            .map_err(|e| CliError::usage(format!("`--workspace {S3_SCHEME}{location}`: {e}")))?;
        Ok(graphrt_files::Workspace::new(Arc::new(store), limits))
    }
    #[cfg(not(feature = "files-s3"))]
    {
        let _ = (limits, vars);
        Err(CliError::usage(format!(
            "`--workspace {S3_SCHEME}{location}` requires a build with the `files-s3` feature"
        )))
    }
}

/// Register the `documents` tool provider over the workspace (`--document-tools`).
#[cfg(feature = "documents")]
fn install_document_tools(
    options: &WorkspaceOptions,
    workspace: &graphrt_files::Workspace,
    tools: &mut graphrt_core::tool::ToolRegistry,
) -> Result<(), CliError> {
    use graphrt_docgen::{DocumentTools, PROVIDER};
    if !options.document_tools {
        return Ok(());
    }
    if tools.get(PROVIDER).is_some() {
        return Err(CliError::usage(format!(
            "tool provider `{PROVIDER}` is defined by a plugin and by `--document-tools`; rename the plugin's provider"
        )));
    }
    let document_tools = DocumentTools::new(workspace.clone());
    let document_tools = if options.per_user { document_tools.scoped_by_context() } else { document_tools };
    tools.add(PROVIDER, Arc::new(document_tools));
    Ok(())
}

/// Register the `image` tool provider over the workspace (`--image-tools`),
/// with a generator when `--image-model` names one.
#[cfg(feature = "image")]
fn install_image_tools(
    options: &WorkspaceOptions,
    vars: &[(String, String)],
    workspace: &graphrt_files::Workspace,
    tools: &mut graphrt_core::tool::ToolRegistry,
) -> Result<(), CliError> {
    use graphrt_image::{ImageTools, PROVIDER};
    if !options.image_tools {
        return Ok(());
    }
    if tools.get(PROVIDER).is_some() {
        return Err(CliError::usage(format!(
            "tool provider `{PROVIDER}` is defined by a plugin and by `--image-tools`; rename the plugin's provider"
        )));
    }
    let mut image_tools = ImageTools::new(workspace.clone());
    if let Some(model) = &options.image_model {
        image_tools = image_tools.with_generator(image_generator(model, vars)?);
    }
    if options.per_user {
        image_tools = image_tools.scoped_by_context();
    }
    tools.add(PROVIDER, Arc::new(image_tools));
    Ok(())
}

/// The generator behind `image/generate` for `--image-model [PROVIDER/]NAME`:
/// `openai` (the default) or `gemini`, with the key and endpoint of that
/// provider from the environment or the env file.
///
/// @throws CliError (usage) - A missing key.
#[cfg(feature = "image")]
fn image_generator(model: &str, vars: &[(String, String)]) -> Result<Arc<dyn graphrt_image::ImageGenerator>, CliError> {
    let lookup = |name: &str| {
        let found = vars.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value.clone());
        found.filter(|value| !value.is_empty())
    };
    // Only the two known prefixes name a provider. Anything else is a model name
    // of an OpenAI compatible server, which may hold a slash itself.
    let (provider, name) = match model.split_once('/') {
        Some((provider @ ("openai" | "gemini"), name)) => (provider, name),
        _ => ("openai", model),
    };
    if provider == "gemini" {
        let key = lookup("GEMINI_API_KEY").or_else(|| lookup("GOOGLE_API_KEY")).ok_or_else(|| {
            CliError::usage(
                "`--image-model gemini/...` needs `GEMINI_API_KEY` or `GOOGLE_API_KEY` in the environment or the \
                 env file",
            )
        })?;
        let base_url =
            lookup("GEMINI_BASE_URL").unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_owned());
        return Ok(Arc::new(graphrt_image::GeminiImages::new(base_url, key, name)));
    }
    let key = lookup("OPENAI_API_KEY")
        .ok_or_else(|| CliError::usage("`--image-model` needs `OPENAI_API_KEY` in the environment or the env file"))?;
    let base_url = lookup("OPENAI_BASE_URL").unwrap_or_else(|| "https://api.openai.com/v1".to_owned());
    Ok(Arc::new(graphrt_image::OpenAiImages::new(base_url, key, name)))
}

/// Add every provider of `extra` to `base`; a provider name used by both is an error.
#[cfg(feature = "mcp")]
fn merge_tools(
    mut base: graphrt_core::tool::ToolRegistry,
    extra: &graphrt_core::tool::ToolRegistry,
) -> Result<graphrt_core::tool::ToolRegistry, CliError> {
    for (name, runtime) in extra.providers() {
        if base.get(name).is_some() {
            return Err(CliError::usage(format!(
                "tool provider `{name}` is defined by both a plugin and an MCP server; rename one of them"
            )));
        }
        base.add(name, runtime.clone());
    }
    Ok(base)
}

/// Values for the workflow's declared env variables, taken from the env file and the
/// process environment (env file wins). Undeclared variables are never exposed.
pub fn workflow_env(graph: &Graph, options: &ServiceOptions) -> IndexMap<String, Value> {
    let vars = options.all_vars();
    let mut env = IndexMap::new();
    for decl in &graph.workflow().env {
        if let Some((_, value)) = vars.iter().rev().find(|(k, _)| *k == decl.name) {
            env.insert(decl.name.clone(), Value::from(value.as_str()));
        }
    }
    env
}

/// Limits from CLI flags.
pub fn limits(timeout_ms: Option<u64>, max_steps: Option<u64>) -> Limits {
    let mut limits = Limits::default();
    if let Some(ms) = timeout_ms {
        limits.max_duration_ms = Some(ms);
    }
    if let Some(steps) = max_steps {
        limits.max_steps = steps;
    }
    limits
}

/// Parse a `.env` file: `KEY=VALUE` lines, `#` comments, optional `export ` prefix
/// and optional single or double quotes around the value.
pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let unquoted = strip_quotes(value, '"').or_else(|| strip_quotes(value, '\'')).unwrap_or(value);
        out.push((key.to_owned(), unquoted.to_owned()));
    }
    out
}

fn strip_quotes(value: &str, quote: char) -> Option<&str> {
    value.strip_prefix(quote).and_then(|v| v.strip_suffix(quote))
}

/// Parse a `--input` style `key=value` pair.
///
/// - `key=value` passes `value` as a string (the start node converts it to the declared type),
///   except that values starting with `[` or `{` are parsed as JSON;
/// - `key:=json` always parses the right-hand side as JSON.
pub fn parse_assignment(raw: &str) -> Result<(String, Value), CliError> {
    if let Some((key, json)) = raw.split_once(":=")
        && !key.contains('=')
    {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| CliError::usage(format!("invalid JSON for `{key}`: {e}")))?;
        return Ok((key.to_owned(), Value::from_json(value)));
    }
    let (key, value) =
        raw.split_once('=').ok_or_else(|| CliError::usage(format!("expected `key=value`, got `{raw}`")))?;
    if key.is_empty() {
        return Err(CliError::usage(format!("missing key in `{raw}`")));
    }
    let trimmed = value.trim_start();
    if (trimmed.starts_with('[') || trimmed.starts_with('{'))
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(value)
    {
        return Ok((key.to_owned(), Value::from_json(json)));
    }
    Ok((key.to_owned(), Value::from(value)))
}

/// File next to a knowledge config holding the datasets created at runtime
/// (`kb.yaml` -> `kb.datasets.json`), so they survive restarts without the
/// operator's config file being rewritten.
#[cfg(feature = "rag")]
pub fn runtime_datasets_path(config: &Path) -> std::path::PathBuf {
    let stem = config.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "knowledge".into());
    config.with_file_name(format!("{stem}.datasets.json"))
}

/// Build the knowledge base declared by `path`, plus the runtime datasets saved
/// next to it. A saved dataset that no longer fits the config (its store or
/// embedder was removed) is skipped with a warning instead of failing startup.
#[cfg(feature = "rag")]
pub fn load_knowledge(path: &Path, vars: &[(String, String)]) -> Result<graphrt_rag::KnowledgeBase, CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::usage(format!("cannot read knowledge config `{}`: {e}", path.display())))?;
    let value = parse_structured(&text, &format!("knowledge config `{}`", path.display()))?;
    let config = graphrt_rag::KnowledgeConfig::from_json(&value.to_string())
        .map_err(|e| CliError::usage(format!("knowledge config `{}`: {e}", path.display())))?;
    let lookup = |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let kb = config
        .build_with_env(&lookup)
        .map_err(|e| CliError::usage(format!("knowledge config `{}`: {e}", path.display())))?;
    let saved = runtime_datasets_path(path);
    if saved.is_file() {
        let text = std::fs::read_to_string(&saved)
            .map_err(|e| CliError::usage(format!("cannot read `{}`: {e}", saved.display())))?;
        let specs: Vec<graphrt_rag::DatasetSpec> =
            serde_json::from_str(&text).map_err(|e| CliError::usage(format!("invalid `{}`: {e}", saved.display())))?;
        for spec in specs {
            let id = spec.id.clone();
            if let Err(e) = kb.create_dataset(spec) {
                eprintln!("warning: runtime dataset `{id}` from {} skipped: {e}", saved.display());
            }
        }
    }
    Ok(kb)
}

/// Persist the knowledge base's runtime datasets next to its config
/// (temp file + rename, so a crash leaves the old or the new list).
#[cfg(feature = "rag")]
pub fn save_runtime_datasets(config: &Path, kb: &graphrt_rag::KnowledgeBase) -> Result<(), CliError> {
    let path = runtime_datasets_path(config);
    let text = serde_json::to_vec_pretty(&kb.runtime_datasets()).map_err(|e| CliError::failed(e.to_string()))?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let io = |p: &Path, e: std::io::Error| CliError::failed(format!("cannot write `{}`: {e}", p.display()));
    std::fs::write(&temp, text).map_err(|e| io(&temp, e))?;
    std::fs::rename(&temp, &path).map_err(|e| {
        let _cleanup = std::fs::remove_file(&temp);
        io(&path, e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_parsing() {
        let vars = parse_env_file("# c\nA=1\nexport B=\"two words\"\nC='x=y'\n\nbad line\n");
        assert_eq!(vars, vec![("A".into(), "1".into()), ("B".into(), "two words".into()), ("C".into(), "x=y".into())]);
    }

    #[test]
    fn model_override_rewrites_model_nodes_only() {
        let mut wf = Workflow::parse(
            "graph:\n  nodes:\n    - {id: s, type: start}\n    - {id: l, type: llm, model: {provider: openai, name: gpt-4o-mini, completion_params: {temperature: 0.1}}}\n    - {id: a, type: agent}\n    - {id: t, type: template-transform, model: {provider: x, name: y}}\n",
        )
        .unwrap();
        let (p, n) = parse_model_override("ollama/library/qwen2.5:14b").unwrap();
        assert_eq!((p.as_str(), n.as_str()), ("ollama", "library/qwen2.5:14b"));
        assert_eq!(apply_model_override(&mut wf, &p, &n), 2);
        let llm = &wf.graph.nodes[1].config["model"];
        assert_eq!(llm["provider"], "ollama");
        assert_eq!(llm["completion_params"]["temperature"], 0.1);
        assert_eq!(wf.graph.nodes[2].config["model"]["name"], "library/qwen2.5:14b");
        assert_eq!(wf.graph.nodes[3].config["model"]["provider"], "x");
        assert!(parse_model_override("nomodel").is_err());
        assert!(parse_model_override("/x").is_err());
    }

    #[test]
    fn assignment_parsing() {
        assert_eq!(parse_assignment("name=Ada").unwrap(), ("name".into(), Value::from("Ada")));
        assert_eq!(parse_assignment("n:=42").unwrap(), ("n".into(), Value::Int(42)));
        assert!(matches!(parse_assignment("xs=[1,2]").unwrap().1, Value::Array(_)));
        assert_eq!(parse_assignment("q=a=b").unwrap(), ("q".into(), Value::from("a=b")));
        assert!(parse_assignment("novalue").is_err());
    }
}
