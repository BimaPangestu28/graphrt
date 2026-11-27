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
                #[allow(unused_mut)]
                let mut own = services_with_skills(&workflow_services, &store);
                #[cfg(feature = "memory")]
                setup::with_memory_defaults(&mut own, options, name);
                bundled_skills.insert(name.clone(), store);
                engine.with_services(own)
            }
            None => with_workflow_services(engine, &workflow_services, options, name),
        };
        #[cfg(not(feature = "skills"))]
        let engine = with_workflow_services(engine, &workflow_services, options, name);
        workflows.insert(name.clone(), ServedWorkflow { engine, env });
    }
    #[cfg(feature = "subagents")]
    link_served(&mut workflows)?;
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
    // `/v1/memory` manages the store the served agents use.
    #[cfg(feature = "memory")]
    let state =
        state.with_memory(services.get::<graphrt_memory::MemoryStore>().cloned(), options.memory_tenant.clone());
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

/// `engine` with the host services, and what only this workflow's runs get:
/// the defaults of its memory scopes. Without memory the services are shared.
fn with_workflow_services(
    engine: Engine,
    services: &Arc<graphrt_core::Services>,
    options: &ServiceOptions,
    name: &str,
) -> Engine {
    #[cfg(feature = "memory")]
    {
        let mut own = (**services).clone();
        setup::with_memory_defaults(&mut own, options, name);
        engine.with_services(own)
    }
    #[cfg(not(feature = "memory"))]
    {
        let _ = (options, name);
        engine.with_shared_services(Arc::clone(services))
    }
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

/// Give each served workflow whose agents list `subagents` the workflows
/// they may start: the ones they list, and what those list in turn. Only
/// those, because every linked workflow is in the fingerprint of a paused
/// run, and a change to an unrelated one must not refuse its resume. A
/// workflow without such an agent is linked to nothing. Each linked
/// workflow brings its own environment and services (memory scope, skills,
/// tools), not the parent's.
///
/// @throws CliError - when an agent lists a workflow this server does not serve,
///   so a misnamed subagent stops the start, not a run halfway.
#[cfg(feature = "subagents")]
pub(crate) fn link_served(workflows: &mut IndexMap<String, ServedWorkflow>) -> Result<(), CliError> {
    let lists: IndexMap<String, Vec<String>> =
        workflows.iter().map(|(name, served)| (name.clone(), crate::setup::listed(served.engine.graph()))).collect();
    let links: IndexMap<String, graphrt_core::engine::LinkedWorkflow> = workflows
        .iter()
        .map(|(name, served)| {
            let link = graphrt_core::engine::LinkedWorkflow::new(Arc::clone(served.engine.graph()))
                .with_env(served.env.clone())
                .with_services(Arc::clone(served.engine.services()));
            (name.clone(), link)
        })
        .collect();
    for (name, names) in &lists {
        for listed in names {
            let Some(link) = links.get(listed) else {
                return Err(CliError::usage(format!(
                    "workflow `{name}` lists subagent `{listed}`, which this server does not serve (served: {})",
                    links.keys().cloned().collect::<Vec<_>>().join(", ")
                )));
            };
            link.check().map_err(|why| {
                CliError::usage(format!("workflow `{name}` lists subagent `{listed}`, which cannot run as one: {why}"))
            })?;
        }
    }
    for (name, served) in workflows.iter_mut() {
        if lists.get(name).is_none_or(Vec::is_empty) {
            continue;
        }
        // Every workflow reachable from this one through the lists.
        let mut reach: Vec<String> = Vec::new();
        let mut queue: Vec<String> = lists.get(name).cloned().unwrap_or_default();
        while let Some(next) = queue.pop() {
            if reach.contains(&next) || !links.contains_key(&next) {
                continue;
            }
            queue.extend(lists.get(&next).cloned().unwrap_or_default());
            reach.push(next);
        }
        let linked: Vec<(String, graphrt_core::engine::LinkedWorkflow)> =
            reach.into_iter().filter_map(|n| links.get(&n).map(|link| (n, link.clone()))).collect();
        let graph = Arc::clone(served.engine.graph());
        let engine = std::mem::replace(&mut served.engine, Engine::new(graph));
        served.engine = engine.with_linked(linked);
    }
    Ok(())
}

#[cfg(all(test, feature = "subagents"))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod subagent_tests {
    use super::*;

    fn served(doc: serde_json::Value) -> ServedWorkflow {
        let registry = graphrt_core::node::Registry::with_builtins();
        let mut registry = registry;
        graphrt_llm::register(&mut registry);
        let graph =
            graphrt_core::graph::Graph::build(graphrt_core::Workflow::from_value(doc).unwrap(), &registry).unwrap();
        ServedWorkflow { engine: Engine::new(Arc::new(graph)), env: IndexMap::new() }
    }

    fn with_agent(list: &[&str]) -> serde_json::Value {
        let subagents: Vec<serde_json::Value> =
            list.iter().map(|w| serde_json::json!({"workflow": w, "description": "d"})).collect();
        serde_json::json!({"graph": {"nodes": [
            {"id": "start", "type": "start"},
            {"id": "lead", "type": "agent", "model": {"provider": "x", "name": "m"}, "subagents": subagents}],
            "edges": [{"source": "start", "target": "lead"}]}})
    }

    #[test]
    fn only_what_is_listed_and_what_that_lists_is_linked() {
        let plain = serde_json::json!({"graph": {"nodes": [{"id": "start", "type": "start"}], "edges": []}});
        let mut workflows = IndexMap::new();
        workflows.insert("lead".to_owned(), served(with_agent(&["middle"])));
        workflows.insert("middle".to_owned(), served(with_agent(&["leaf"])));
        workflows.insert("leaf".to_owned(), served(plain.clone()));
        workflows.insert("other".to_owned(), served(plain));
        let before_other = workflows["other"].engine.fingerprint().to_owned();
        link_served(&mut workflows).unwrap();
        let lead = workflows["lead"].engine.fingerprint();
        assert!(lead.contains(":middle=") && lead.contains(":leaf=") && !lead.contains(":other="), "{lead}");
        assert_eq!(workflows["other"].engine.fingerprint(), before_other, "a workflow without subagents is not linked");
        // A listed workflow the server does not serve stops the start.
        let mut workflows = IndexMap::new();
        workflows.insert("lead".to_owned(), served(with_agent(&["researcher"])));
        let error = link_served(&mut workflows).unwrap_err().to_string();
        assert!(error.contains("`researcher`"), "{error}");
        // A served workflow that a model cannot start stops the start too, unless
        // the variable it cannot pass has a default.
        let start = |default: serde_json::Value| {
            serde_json::json!({"graph": {"nodes": [{"id": "start", "type": "start", "variables": [
                {"variable": "token", "type": "secret", "required": true, "default": default}]}], "edges": []}})
        };
        let mut workflows = IndexMap::new();
        workflows.insert("lead".to_owned(), served(with_agent(&["locked"])));
        workflows.insert("locked".to_owned(), served(start(serde_json::Value::Null)));
        let error = link_served(&mut workflows).unwrap_err().to_string();
        assert!(error.contains("cannot run as one") && error.contains("`token`"), "{error}");
        workflows.insert("locked".to_owned(), served(start(serde_json::json!("fallback"))));
        link_served(&mut workflows).unwrap();
    }
}
