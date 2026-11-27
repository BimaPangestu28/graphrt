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
    #[cfg(feature = "skills")]
    let mut bundled_skills = IndexMap::new();
    for (name, path) in files {
        if path.extension().and_then(|e| e.to_str()) == Some("json") && is_mcp_config(path) {
            continue;
        }
        // A bundle that brings its own skills runs on them and on no others.
        #[cfg(feature = "skills")]
        let mut own_skills = None;
        let mut own_services: Option<Arc<graphrt_core::Services>> = None;
        let graph = if crate::bundle_cmd::is_bundle(path) {
            // Bundled WebAssembly plugins are registered; services come from the host flags.
            let mut bundle_options = options.clone();
            // Left as it is by a bundle without skills, so what comes back is the bundle's own store.
            bundle_options.skills_dir = None;
            let graph = crate::bundle_cmd::bundle_graph(
                path,
                &mut bundle_options,
                &mut extensions.registry,
                &options.trust_keys,
            )?;
            // A bundle that ships its own providers or knowledge config (and the host gave
            // none) runs on services built from it, so its knowledge-retrieval steps search
            // the passages that came with it.
            if bundle_options.knowledge != options.knowledge || bundle_options.providers != options.providers {
                own_services = Some(Arc::new(setup::services(&bundle_options, extensions.tools.clone()).await?));
            }
            #[cfg(feature = "skills")]
            {
                own_skills = bundle_options.skills_dir.map(|dir| Arc::new(graphrt_skill::SkillStore::open(dir)));
            }
            graph
        } else {
            setup::load_graph_with(path, options.model_override.as_deref(), &extensions.registry)?
        };
        let env = setup::workflow_env(&graph, options);
        let engine = Engine::new(graph).with_clock(Arc::new(TokioClock)).with_limits(limits.clone());
        let workflow_services = own_services.unwrap_or_else(|| Arc::clone(&services));
        #[cfg(feature = "skills")]
        let engine = match own_skills {
            Some(store) => {
                let engine = engine.with_services(services_with_skills(&workflow_services, &store));
                bundled_skills.insert(name.clone(), store);
                engine
            }
            None => engine.with_shared_services(workflow_services),
        };
        #[cfg(not(feature = "skills"))]
        let engine = engine.with_shared_services(workflow_services);
        workflows.insert(name.clone(), ServedWorkflow { engine, env });
    }
    #[allow(unused_mut)]
    let mut state = AppState::new(workflows, api_key).with_extensions(extensions);
    #[cfg(any(feature = "online-eval", feature = "skills", feature = "learning"))]
    {
        state.services = Some(Arc::clone(&services));
    }
    // `/v1/skills` manages the store the served agents read. Skills that came in a
    // bundle are listed there and cannot be changed: the bundle pins them.
    #[cfg(feature = "skills")]
    let state = {
        let state = match &options.skills_dir {
            Some(dir) => state.with_skill_store(Some(Arc::new(graphrt_skill::SkillStore::open(dir)))),
            None => state,
        };
        state.with_bundled_skills(bundled_skills)
    };
    // `/v1/learning` manages the lessons the served agents read.
    #[cfg(feature = "learning")]
    let state = match &options.learning_dir {
        Some(dir) => state.with_lesson_store(Some(Arc::new(graphrt_learn::LessonStore::open(dir)))),
        None => state,
    };
    // The same workspace the `files` tools use, so `/v1/files` shares its quota.
    #[cfg(feature = "files")]
    let state = state.with_workspace(services.get::<setup::ServedWorkspace>().cloned());
    // The same knowledge base the workflows use, so documents added over the API
    // are searchable by running workflows right away.
    #[cfg(all(feature = "rag", not(feature = "connectors")))]
    let state = state.with_knowledge(services.get::<graphrt_rag::KnowledgeBase>().cloned(), options.knowledge.clone());
    // With connectors, the `sources:` of the config come along (scheduled once serving starts).
    #[cfg(feature = "connectors")]
    let state = state.with_knowledge_sources(
        services.get::<graphrt_rag::KnowledgeBase>().cloned(),
        options.knowledge.clone(),
        &options.all_vars(),
    )?;
    Ok(state)
}

/// The host services with the skill source replaced by `store`, for the engine
/// of a bundle that brings its own skills. Every other service is shared.
#[cfg(feature = "skills")]
fn services_with_skills(
    host: &graphrt_core::Services,
    store: &Arc<graphrt_skill::SkillStore>,
) -> graphrt_core::Services {
    let mut own = host.clone();
    own.insert(graphrt_skill::SkillSet::new(Arc::clone(store)).into_service());
    own
}

fn is_mcp_config(path: &Path) -> bool {
    std::fs::read_to_string(path).map(|t| t.contains("\"mcpServers\"")).unwrap_or(false)
}
