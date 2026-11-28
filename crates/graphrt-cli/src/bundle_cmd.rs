//! Bundles (`.grt`): `graphrt bundle build|inspect|verify|sign|keygen`, and
//! running / serving bundles directly.
//!
//! A bundle carries a workflow plus its WebAssembly plugins, knowledge-base
//! config and data, and a providers file without secrets. `graphrt run app.grt`
//! verifies it, registers its plugins and uses its configs unless the matching
//! flags (`--providers`, `--knowledge`) are given.

use std::path::Path;
#[cfg(feature = "bundle")]
use std::path::PathBuf;
use std::sync::Arc;

use graphrt_core::{Graph, Registry};

use crate::error::CliError;
#[cfg(feature = "bundle")]
use crate::error::EXIT_OK;
#[cfg(feature = "bundle")]
use crate::setup;
use crate::setup::ServiceOptions;

/// Comma-separated hex public keys whose bundle signatures are trusted, in
/// addition to `--trust-key`. When any key is configured, bundles must be signed
/// by one of them.
pub const TRUSTED_KEYS_ENV: &str = "GRAPHRT_TRUSTED_KEYS";

/// Whether `path` names a bundle (`.grt`).
pub fn is_bundle(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("grt"))
}

/// Trusted signing keys from flags and `$GRAPHRT_TRUSTED_KEYS`.
pub fn trusted_keys(flags: &[String]) -> Vec<String> {
    let mut keys: Vec<String> = flags.to_vec();
    if let Ok(list) = std::env::var(TRUSTED_KEYS_ENV) {
        keys.extend(list.split(',').map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned));
    }
    keys
}

/// What a bundle runs: its entry workflow, the workflows its agents start as
/// subagents (by name), and the budgets of its runs.
#[derive(Debug)]
pub struct BundleParts {
    pub entry: Arc<Graph>,
    pub helpers: Vec<(String, Arc<Graph>)>,
    /// `max_total_tokens` and `max_cost_micros` of the manifest's `limits`.
    pub limits: (Option<u64>, Option<u64>),
}

impl BundleParts {
    /// `host` with the bundle's budgets: the lower of the two where both set one.
    pub fn limits_over(&self, host: &graphrt_core::engine::Limits) -> graphrt_core::engine::Limits {
        let lower = |host: Option<u64>, bundle: Option<u64>| match (host, bundle) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let mut limits = host.clone();
        limits.max_total_tokens = lower(host.max_total_tokens, self.limits.0);
        limits.max_cost_micros = lower(host.max_cost_micros, self.limits.1);
        limits
    }
}

/// Open a bundle, register its WebAssembly plugins in `registry` and compile its
/// entry workflow. When `options` has no `--providers` / `--knowledge`, the
/// bundle's own configs are used (extracted to a private temporary directory).
pub fn bundle_graph(
    path: &Path,
    options: &mut ServiceOptions,
    registry: &mut Registry,
    trust: &[String],
) -> Result<Arc<Graph>, CliError> {
    Ok(bundle_parts(path, options, registry, trust)?.entry)
}

/// [`bundle_graph`], with the workflows the bundle carries for its subagents
/// (every workflow besides the entry, compiled the same way) and its budgets.
#[cfg(feature = "bundle")]
pub fn bundle_parts(
    path: &Path,
    options: &mut ServiceOptions,
    registry: &mut Registry,
    trust: &[String],
) -> Result<BundleParts, CliError> {
    let bundle = open(path, trust)?;
    let kinds = bundle
        .register_wasm_plugins(registry, &graphrt_bundle::WasmLimits::default())
        .map_err(|e| CliError::invalid(format!("{}: {e}", path.display())))?;
    if !kinds.is_empty() {
        eprintln!("bundle: registered plugin node kinds {}", kinds.join(", "));
    }
    let report = bundle.check(registry);
    for plugin in &report.non_portable_plugins {
        eprintln!("warning: bundle plugin `{plugin}` is not WebAssembly and is not loaded from the bundle");
    }
    if let Some(mismatch) = &report.version_mismatch {
        return Err(CliError::invalid(format!("{}: {mismatch}", path.display())));
    }
    if bundle.providers_config().is_some() || bundle.knowledge_config().is_some() {
        apply_bundle_configs(&bundle, options)?;
    }
    // A bundle that brings skills runs with those skills and no others.
    #[cfg(feature = "skills")]
    if bundle.file(crate::bundle_skills::SKILL_INDEX_PATH).is_some() {
        let fingerprint = graphrt_bundle::sha256_hex(bundle.manifest_bytes());
        let dir = create_private_dir(&format!("skills-{}", fingerprint.get(..16).unwrap_or(&fingerprint)))?;
        // `open` refused a signer outside the trusted keys when there are any. Without
        // trusted keys a signature only says that someone signed it.
        let signed_by_trusted_key = bundle.signer().is_some() && !trusted_keys(trust).is_empty();
        options.skills_dir = crate::bundle_skills::install_from_bundle(&bundle, &dir, signed_by_trusted_key)?;
    }
    let compile = |name: Option<&str>| -> Result<Arc<Graph>, CliError> {
        let mut workflow = bundle.workflow(name).map_err(|e| CliError::invalid(format!("{}: {e}", path.display())))?;
        if let Some(raw) = options.model_override.as_deref() {
            let (provider, model) = setup::parse_model_override(raw)?;
            setup::apply_model_override(&mut workflow, &provider, &model);
        }
        let graph = Graph::build(workflow, registry).map_err(|e| {
            CliError::invalid(crate::extensions::explain_unknown_kind(format!("{}: {e}", path.display())))
        })?;
        Ok(Arc::new(graph))
    };
    let entry = compile(None)?;
    let manifest = bundle.manifest();
    let mut helpers = Vec::new();
    for stored in manifest.workflows.iter().filter(|stored| **stored != manifest.entry) {
        let name = stored.rsplit('/').next().unwrap_or(stored);
        let name = name.rsplit_once('.').map_or(name, |(stem, _)| stem).to_owned();
        helpers.push((name, compile(Some(stored))?));
    }
    let limits = manifest.limits.map_or((None, None), |limits| (limits.max_total_tokens, limits.max_cost_micros));
    Ok(BundleParts { entry, helpers, limits })
}

/// Without the `bundle` feature, bundles are rejected with a clear message.
#[cfg(not(feature = "bundle"))]
pub fn bundle_parts(
    path: &Path,
    _options: &mut ServiceOptions,
    _registry: &mut Registry,
    _trust: &[String],
) -> Result<BundleParts, CliError> {
    Err(CliError::usage(format!("`{}`: running bundles requires a build with the `bundle` feature", path.display())))
}

#[cfg(feature = "bundle")]
fn open(path: &Path, trust: &[String]) -> Result<graphrt_bundle::Bundle, CliError> {
    let mut opts = graphrt_bundle::OpenOptions::default();
    for key in trusted_keys(trust) {
        opts = opts.trust_key(key);
    }
    graphrt_bundle::Bundle::open_path(path, &opts).map_err(|e| CliError::invalid(format!("{}: {e}", path.display())))
}

/// Extract the bundle into a private temporary directory and point the providers
/// and knowledge options at its configs (only where the user gave none).
#[cfg(feature = "bundle")]
fn apply_bundle_configs(bundle: &graphrt_bundle::Bundle, options: &mut ServiceOptions) -> Result<(), CliError> {
    let fingerprint = graphrt_bundle::sha256_hex(bundle.manifest_bytes());
    let dir = create_private_dir(fingerprint.get(..16).unwrap_or(&fingerprint))?;
    bundle.extract_to(&dir).map_err(|e| CliError::failed(format!("cannot extract bundle: {e}")))?;
    if options.providers.is_none()
        && let Some((rel, _)) = bundle.providers_config()
    {
        options.providers = Some(dir.join(rel));
    }
    if options.knowledge.is_none()
        && let Some(config) = bundle.knowledge_config_at(&dir)
    {
        let config = config.map_err(|e| CliError::invalid(e.to_string()))?;
        let target = dir.join("knowledge/resolved.json");
        let text = serde_json::to_vec_pretty(&config).map_err(|e| CliError::failed(e.to_string()))?;
        std::fs::write(&target, text)
            .map_err(|e| CliError::failed(format!("cannot write `{}`: {e}", target.display())))?;
        options.knowledge = Some(target);
    }
    Ok(())
}

/// Create a new, owner-only (`0700` on Unix) directory in the temp dir for extracted
/// bundle configs. The name is unique per invocation and creation fails if the path
/// exists, so another local user cannot pre-create it (or plant symlinks in it) to
/// redirect writes or swap `providers.json` between extraction and use.
#[cfg(feature = "bundle")]
fn create_private_dir(label: &str) -> Result<std::path::PathBuf, CliError> {
    let prefix = format!("graphrt-bundle-{label}");
    graphrt_core::private_fs::create_fresh_temp_dir(&prefix)
        .map_err(|e| CliError::failed(format!("cannot create a `{prefix}` temp directory: {e}")))
}

/// Execute `graphrt bundle ...`.
#[cfg(feature = "bundle")]
pub fn bundle(args: crate::args::BundleArgs) -> Result<u8, CliError> {
    use crate::args::BundleCommand;
    match args.command {
        BundleCommand::Build(build) => build_cmd(*build),
        BundleCommand::Inspect { bundle, json, trust_key } => inspect(&bundle, json, &trust_key),
        BundleCommand::Verify { bundle, trust_key } => {
            let opened = open(&bundle, &trust_key)?;
            let signer = opened.signer().map_or_else(|| "unsigned".to_owned(), |k| format!("signed by {k}"));
            println_out(&format!("{}: ok ({} files, {signer})", bundle.display(), opened.manifest().files.len()))?;
            Ok(EXIT_OK)
        }
        BundleCommand::Sign { bundle, key, out } => {
            let secret = read_secret_key(&key)?;
            let bytes = std::fs::read(&bundle)
                .map_err(|e| CliError::usage(format!("cannot read `{}`: {e}", bundle.display())))?;
            let signed = graphrt_bundle::sign_bundle(&bytes, &secret)
                .map_err(|e| CliError::invalid(format!("{}: {e}", bundle.display())))?;
            let target = out.unwrap_or(bundle);
            write_file(&target, &signed)?;
            println_out(&format!("signed {}", target.display()))?;
            Ok(EXIT_OK)
        }
        BundleCommand::Keygen { out } => {
            let pair = graphrt_bundle::generate_keypair().map_err(|e| CliError::failed(e.to_string()))?;
            if out.exists() {
                return Err(CliError::usage(format!(
                    "`{}` already exists; refusing to overwrite a key",
                    out.display()
                )));
            }
            write_secret(&out, &pair.secret_key)?;
            println_out(&format!("secret key written to {} (keep it private)", out.display()))?;
            println_out(&format!("public key: {}", pair.public_key))?;
            Ok(EXIT_OK)
        }
    }
}

#[cfg(feature = "bundle")]
fn build_cmd(args: crate::args::BundleBuildArgs) -> Result<u8, CliError> {
    let text = std::fs::read_to_string(&args.workflow)
        .map_err(|e| CliError::usage(format!("cannot read `{}`: {e}", args.workflow.display())))?;
    let workflow = graphrt_core::Workflow::parse(&text)
        .map_err(|e| CliError::invalid(format!("{}: {e}", args.workflow.display())))?;
    let stem = args.workflow.file_stem().and_then(|s| s.to_str()).unwrap_or("workflow").to_owned();
    let name = args.name.clone().or_else(|| workflow.name.clone()).unwrap_or_else(|| stem.clone());
    let ext = args.workflow.extension().and_then(|e| e.to_str()).unwrap_or("yaml");
    let entry = format!("workflow.{ext}");

    let invalid = |e: graphrt_bundle::BundleError| CliError::invalid(e.to_string());
    let mut builder =
        graphrt_bundle::BundleBuilder::new(&name, &args.bundle_version).workflow(&entry, text).map_err(invalid)?;
    if let Some(description) = workflow.description.clone().filter(|d| !d.is_empty()) {
        builder = builder.description(description);
    }
    // The workflows its agents start as subagents, and what those start, travel with it.
    let folder = match &args.workflows {
        Some(dir) => dir.clone(),
        None => args.workflow.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    let helpers = helper_workflows(&workflow, &stem, &folder)?;
    for helper in &helpers {
        builder = builder.workflow(&helper.bundle_path, helper.text.clone()).map_err(invalid)?;
    }
    builder = builder.limits(graphrt_bundle::BundleLimits {
        max_total_tokens: args.max_tokens_per_run,
        max_cost_micros: args.max_cost_per_run,
    });
    let workflows: Vec<&graphrt_core::Workflow> =
        std::iter::once(&workflow).chain(helpers.iter().map(|helper| &helper.workflow)).collect();

    // Explicit plugin directories.
    let mut included: Vec<String> = Vec::new();
    for dir in &args.plugin {
        builder = builder.plugin_dir(dir).map_err(invalid)?;
        included.push(dir.display().to_string());
    }
    // Node kinds this build does not know: look for WebAssembly plugins providing them.
    let extensions = crate::extensions::Extensions::load(&args.plugins.options())?;
    let builtin = setup::builtin_registry();
    let explicit = explicit_kinds(&args.plugin)?;
    let mut missing = Vec::new();
    let mut seen_plugins: Vec<PathBuf> = Vec::new();
    for node in workflows.iter().flat_map(|workflow| workflow.graph.nodes.iter()) {
        if builtin.contains(&node.kind) || explicit.contains(&node.kind) {
            continue;
        }
        let provider = extensions.plugins.iter().find(|p| p.error.is_none() && p.nodes.iter().any(|k| k == &node.kind));
        match provider {
            Some(p) if p.runtime == "wasm" => {
                if seen_plugins.contains(&p.path) {
                    continue;
                }
                seen_plugins.push(p.path.clone());
                if p.path.extension().and_then(|e| e.to_str()) == Some("wasm") {
                    let bytes = std::fs::read(&p.path)
                        .map_err(|e| CliError::usage(format!("cannot read `{}`: {e}", p.path.display())))?;
                    builder = builder.wasm_module(&p.name, bytes).map_err(invalid)?;
                } else if let Some(dir) = p.path.parent() {
                    builder = builder.plugin_dir(dir).map_err(invalid)?;
                }
                included.push(format!("{} (auto)", p.name));
            }
            Some(p) => {
                eprintln!(
                    "warning: node kind `{}` comes from {} plugin `{}`; pass `--plugin <dir>` to include it (not portable)",
                    node.kind, p.runtime, p.name
                );
                missing.push(node.kind.clone());
            }
            None => missing.push(node.kind.clone()),
        }
    }
    missing.sort_unstable();
    missing.dedup();
    if !missing.is_empty() {
        eprintln!("warning: no plugin found for node kinds: {} (hosts must provide them)", missing.join(", "));
    }

    if let Some(path) = &args.providers {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CliError::usage(format!("cannot read `{}`: {e}", path.display())))?;
        builder = builder.providers(&text).map_err(invalid)?;
    }
    if let Some(path) = &args.knowledge {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CliError::usage(format!("cannot read `{}`: {e}", path.display())))?;
        // Store paths in knowledge configs are relative to the working directory.
        builder = builder.knowledge(&text, Path::new(".")).map_err(invalid)?;
    }
    for dir in &args.include {
        let prefix = dir.file_name().and_then(|n| n.to_str()).map(|n| format!("assets/{n}")).unwrap_or("assets".into());
        builder = builder.include_dir(dir, &prefix).map_err(invalid)?;
    }
    // Skills the agents of every workflow list travel with the bundle.
    #[cfg(feature = "skills")]
    let bundled_skills = {
        let mut names: Vec<String> = Vec::new();
        for listed in workflows.iter().flat_map(|workflow| crate::bundle_skills::listed_skills(workflow)) {
            if !names.contains(&listed) {
                names.push(listed);
            }
        }
        names
    };
    #[cfg(feature = "skills")]
    if !bundled_skills.is_empty() {
        let root = graphrt_skill::SkillStore::default_root()
            .ok_or_else(|| CliError::usage("cannot find the skill store; set GRAPHRT_SKILLS_DIR"))?;
        let store = graphrt_skill::SkillStore::open(root);
        for (path, content) in crate::bundle_skills::files_for_bundle(&store, &bundled_skills)? {
            builder = builder.file(&path, content).map_err(invalid)?;
        }
    }

    let mut bytes = builder.build().map_err(invalid)?;
    if let Some(key) = &args.sign_key {
        let secret = read_secret_key(key)?;
        bytes = graphrt_bundle::sign_bundle(&bytes, &secret).map_err(invalid)?;
    }
    let out = args.out.clone().unwrap_or_else(|| args.workflow.with_file_name(format!("{stem}.grt")));
    write_file(&out, &bytes)?;
    let signed = if args.sign_key.is_some() { ", signed" } else { "" };
    println_out(&format!("built {} ({} bytes{signed})", out.display(), bytes.len()))?;
    for helper in &helpers {
        println_out(&format!("  subagent: {} ({})", helper.name, helper.bundle_path))?;
    }
    for plugin in included {
        println_out(&format!("  plugin: {plugin}"))?;
    }
    #[cfg(feature = "skills")]
    for skill in bundled_skills {
        println_out(&format!("  skill: {skill}"))?;
    }
    Ok(EXIT_OK)
}

/// A workflow an agent of the bundle starts as a subagent.
#[cfg(feature = "bundle")]
#[derive(Debug)]
struct Helper {
    /// The name agents list it under: its file name without the extension.
    name: String,
    /// Where it is kept in the bundle: `workflows/<file name>`.
    bundle_path: String,
    text: String,
    workflow: graphrt_core::Workflow,
}

/// The workflows `entry` lists as subagents, and what those list in turn,
/// read from `folder` by file name (`<name>.yaml`, `.yml` or `.json`).
///
/// @throws CliError (usage) - a listed workflow that is not in `folder`, two
///   files of one name, or a name that is the entry's own (`entry_name`).
/// @throws CliError (invalid) - a listed workflow that does not parse.
#[cfg(feature = "bundle")]
fn helper_workflows(entry: &graphrt_core::Workflow, entry_name: &str, folder: &Path) -> Result<Vec<Helper>, CliError> {
    let mut helpers: Vec<Helper> = Vec::new();
    let mut queue: Vec<(String, String)> =
        setup::listed_in(entry).into_iter().map(|name| ("the workflow".to_owned(), name)).collect();
    queue.reverse();
    while let Some((by, name)) = queue.pop() {
        if name == entry_name || helpers.iter().any(|helper| helper.name == name) {
            continue;
        }
        let valid = !name.is_empty()
            && !name.starts_with('.')
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !valid {
            return Err(CliError::usage(format!("{by} lists subagent `{name}`, which is no workflow file name")));
        }
        // The entry is kept as `workflow.<ext>`: a subagent of that name could not be told apart.
        if name == "workflow" {
            return Err(CliError::usage(format!(
                "{by} lists subagent `workflow`, a name a bundle keeps for its entry: rename that workflow file"
            )));
        }
        let found: Vec<PathBuf> = ["yaml", "yml", "json"]
            .iter()
            .map(|ext| folder.join(format!("{name}.{ext}")))
            .filter(|path| path.is_file())
            .collect();
        let path = match found.as_slice() {
            [path] => path.clone(),
            [] => {
                return Err(CliError::usage(format!(
                    "{by} lists subagent `{name}`, but `{}` has no `{name}.yaml`, `.yml` or `.json` \
                     (pass the folder of its workflow with --workflows DIR)",
                    folder.display()
                )));
            }
            _ => return Err(CliError::usage(format!("`{}` has two workflow files named `{name}`", folder.display()))),
        };
        let text = std::fs::read_to_string(&path)
            .map_err(|e| CliError::usage(format!("cannot read `{}`: {e}", path.display())))?;
        let workflow =
            graphrt_core::Workflow::parse(&text).map_err(|e| CliError::invalid(format!("{}: {e}", path.display())))?;
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
        let lister = format!("`{name}`");
        let mut next: Vec<(String, String)> =
            setup::listed_in(&workflow).into_iter().map(|listed| (lister.clone(), listed)).collect();
        next.reverse();
        queue.extend(next);
        helpers.push(Helper { name, bundle_path: format!("workflows/{file_name}"), text, workflow });
    }
    Ok(helpers)
}

/// Node kinds declared by explicitly passed plugin directories.
#[cfg(feature = "bundle")]
fn explicit_kinds(dirs: &[PathBuf]) -> Result<Vec<String>, CliError> {
    if dirs.is_empty() {
        return Ok(Vec::new());
    }
    let options = crate::extensions::PluginOptions { dirs: dirs.to_vec(), no_default: true, ..Default::default() };
    let ext = crate::extensions::Extensions::load(&options)?;
    Ok(ext.plugins.iter().flat_map(|p| p.nodes.clone()).collect())
}

#[cfg(feature = "bundle")]
fn inspect(path: &Path, json: bool, trust: &[String]) -> Result<u8, CliError> {
    let bundle = open(path, trust)?;
    let mut registry = setup::builtin_registry();
    let plugin_error = bundle
        .register_wasm_plugins(&mut registry, &graphrt_bundle::WasmLimits::default())
        .err()
        .map(|e| e.to_string());
    let report = bundle.check(&registry);
    if json {
        let value = serde_json::json!({
            "manifest": bundle.manifest(),
            "signer": bundle.signer(),
            "compatibility": report,
            "plugin_error": plugin_error,
        });
        let text = serde_json::to_string_pretty(&value).map_err(|e| CliError::failed(e.to_string()))?;
        println_out(&text)?;
        return Ok(EXIT_OK);
    }
    let m = bundle.manifest();
    let mut lines = vec![
        format!("name:      {} {}", m.name, m.version),
        format!("created:   {}", m.created_at),
        format!("entry:     {}", m.entry),
        format!("signature: {}", bundle.signer().map_or("unsigned".to_owned(), |k| format!("valid, key {k}"))),
        format!("files:     {}", m.files.len()),
        format!("nodes:     {}", m.requires.node_kinds.join(", ")),
    ];
    if !m.description.is_empty() {
        lines.insert(1, format!("about:     {}", m.description));
    }
    for p in &m.requires.plugins {
        let portable = if p.portable { "portable" } else { "NOT portable" };
        lines.push(format!("plugin:    {} {} ({}, {portable})", p.name, p.version, p.runtime));
    }
    for helper in m.workflows.iter().filter(|stored| **stored != m.entry) {
        lines.push(format!("subagent:  {helper}"));
    }
    if let Some(limits) = &m.limits {
        let tokens = limits.max_total_tokens.map(|n| format!("{n} tokens"));
        #[allow(clippy::cast_precision_loss)]
        let cost = limits.max_cost_micros.map(|micros| format!("cost {}", micros as f64 / 1_000_000.0));
        let budgets: Vec<String> = tokens.into_iter().chain(cost).collect();
        lines.push(format!("per run:   {}", budgets.join(", ")));
    }
    if !m.requires.providers.is_empty() {
        lines.push(format!("providers: {} (configure on the host)", m.requires.providers.join(", ")));
    }
    if !m.requires.env.is_empty() {
        lines.push(format!("env:       {} (supply on the host)", m.requires.env.join(", ")));
    }
    if let Some(e) = plugin_error {
        lines.push(format!("plugin error: {e}"));
    }
    if report.is_compatible() {
        lines.push("status:    runs on this build".into());
    } else {
        if !report.missing_node_kinds.is_empty() {
            lines.push(format!("missing:   {}", report.missing_node_kinds.join(", ")));
        }
        if let Some(v) = &report.version_mismatch {
            lines.push(format!("version:   {v}"));
        }
    }
    println_out(&lines.join("\n"))?;
    Ok(EXIT_OK)
}

#[cfg(feature = "bundle")]
fn read_secret_key(path: &Path) -> Result<String, CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::usage(format!("cannot read key `{}`: {e}", path.display())))?;
    Ok(text.trim().to_owned())
}

#[cfg(feature = "bundle")]
fn write_secret(path: &Path, secret: &str) -> Result<(), CliError> {
    let io = |e: std::io::Error| CliError::failed(format!("cannot write `{}`: {e}", path.display()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).map_err(io)?;
        file.write_all(secret.as_bytes()).map_err(io)?;
        file.write_all(b"\n").map_err(io)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, format!("{secret}\n")).map_err(io)
    }
}

#[cfg(feature = "bundle")]
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    std::fs::write(path, bytes).map_err(|e| CliError::failed(format!("cannot write `{}`: {e}", path.display())))
}

#[cfg(feature = "bundle")]
fn println_out(text: &str) -> Result<(), CliError> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{text}").map_err(|e| CliError::failed(e.to_string()))
}

#[cfg(all(test, feature = "bundle"))]
mod tests {
    use super::*;

    #[test]
    fn extraction_dirs_are_fresh_and_private() {
        let first = create_private_dir("test").unwrap();
        let second = create_private_dir("test").unwrap();
        assert_ne!(first, second, "every invocation gets its own directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&first).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "extraction dir must be owner-only: {mode:o}");
        }
        for dir in [first, second] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
