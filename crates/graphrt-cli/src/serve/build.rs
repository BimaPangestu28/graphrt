//! Loading served workflows: discover the files, compile them once and build
//! the shared host services they run on.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use graphrt_core::Engine;
use graphrt_core::engine::Limits;
use graphrt_core::node::TokioClock;
use indexmap::IndexMap;

use super::state::{AppState, ServedWorkflow};
use crate::error::CliError;
use crate::extensions::Extensions;
use crate::setup::{self, ServiceOptions};

/// Discover workflow files: a single file, or every `.yaml` / `.yml` / `.json` file in a directory.
pub fn discover(path: &Path) -> Result<Vec<(String, PathBuf)>, CliError> {
    let name_of = |p: &Path| p.file_stem().and_then(|s| s.to_str()).unwrap_or("workflow").to_owned();
    if path.is_file() {
        return Ok(vec![(name_of(path), path.to_path_buf())]);
    }
    let entries =
        std::fs::read_dir(path).map_err(|e| CliError::usage(format!("cannot read `{}`: {e}", path.display())))?;
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default();
        if p.is_file()
            && matches!(ext, "yaml" | "yml" | "json" | "grt")
            && !p.to_string_lossy().ends_with(".snapshot.json")
        {
            found.push((name_of(&p), p));
        }
    }
    found.sort_unstable();
    if found.is_empty() {
        return Err(CliError::usage(format!("no workflow files in `{}`", path.display())));
    }
    Ok(found)
}

/// Compile workflows and build services.
pub async fn build_state(
    files: &[(String, PathBuf)],
    options: &ServiceOptions,
    limits: Limits,
    api_key: Option<String>,
) -> Result<AppState, CliError> {
    let mut extensions = Extensions::load(&options.plugins)?;
    let services = Arc::new(setup::services(options, extensions.tools.clone()).await?);
    let mut workflows = IndexMap::new();
    for (name, path) in files {
        if path.extension().and_then(|e| e.to_str()) == Some("json") && is_mcp_config(path) {
            continue;
        }
        let graph = if crate::bundle_cmd::is_bundle(path) {
            // Bundled WebAssembly plugins are registered; services come from the host flags.
            let mut bundle_options = options.clone();
            crate::bundle_cmd::bundle_graph(path, &mut bundle_options, &mut extensions.registry, &options.trust_keys)?
        } else {
            setup::load_graph_with(path, options.model_override.as_deref(), &extensions.registry)?
        };
        let env = setup::workflow_env(&graph, options);
        let engine = Engine::new(graph)
            .with_shared_services(services.clone())
            .with_clock(Arc::new(TokioClock))
            .with_limits(limits.clone());
        workflows.insert(name.clone(), ServedWorkflow { engine, env });
    }
    #[allow(unused_mut)]
    let mut state = AppState::new(workflows, api_key).with_extensions(extensions);
    #[cfg(feature = "online-eval")]
    {
        state.services = Some(Arc::clone(&services));
    }
    // The same knowledge base the workflows use, so documents added over the API
    // are searchable by running workflows right away.
    #[cfg(feature = "rag")]
    let state = state.with_knowledge(services.get::<graphrt_rag::KnowledgeBase>().cloned(), options.knowledge.clone());
    Ok(state)
}

fn is_mcp_config(path: &Path) -> bool {
    std::fs::read_to_string(path).map(|t| t.contains("\"mcpServers\"")).unwrap_or(false)
}
