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
    // The workflows each bundle carries for its subagents, by the bundle's served name.
    #[cfg(feature = "subagents")]
    #[allow(unused_mut)]
    let mut carried: IndexMap<String, IndexMap<String, ServedWorkflow>> = IndexMap::new();
    for (name, path) in files {
        if path.extension().and_then(|e| e.to_str()) == Some("json") && is_mcp_config(path) {
            continue;
        }
        // A bundle that brings its own skills runs on them and on no others.
        #[cfg(feature = "skills")]
        let mut own_skills = None;
        let mut own_services: Option<Arc<graphrt_core::Services>> = None;
        // What else a bundle brings: the workflows of its subagents and its budgets.
        #[cfg(feature = "bundle")]
        let mut bundled: Option<crate::bundle_cmd::BundleParts> = None;
        let graph = if crate::bundle_cmd::is_bundle(path) {
            // Bundled WebAssembly plugins are registered; services come from the host flags.
            let mut bundle_options = options.clone();
            // Left as it is by a bundle without skills, so what comes back is the bundle's own store.
            bundle_options.skills_dir = None;
            #[cfg(feature = "bundle")]
            let graph = {
                let parts = crate::bundle_cmd::bundle_parts(
                    path,
                    &mut bundle_options,
                    &mut extensions.registry,
                    &options.trust_keys,
                )?;
                let entry = Arc::clone(&parts.entry);
                bundled = Some(parts);
                entry
            };
            #[cfg(not(feature = "bundle"))]
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
        #[cfg(not(feature = "bundle"))]
        let engine = Engine::new(graph).with_clock(Arc::new(TokioClock)).with_limits(limits.clone());
        // A bundle's budgets: the lower of the host's and its own.
        #[cfg(feature = "bundle")]
        let engine = Engine::new(graph)
            .with_clock(Arc::new(TokioClock))
            .with_limits(bundled.as_ref().map_or_else(|| limits.clone(), |parts| parts.budgets.over(&limits)));
        let workflow_services = own_services.unwrap_or_else(|| Arc::clone(&services));
        // The workflows of its subagents run on the bundle's services, each in
        // its own memory scope. They are linked to the bundle's agents only, never
        // served on their own: a bundle adds no endpoint besides its entry.
        #[cfg(all(feature = "bundle", feature = "subagents"))]
        if let Some(parts) = bundled.take() {
            let mut helpers = IndexMap::new();
            for (helper, graph) in parts.helpers {
                let env = setup::workflow_env(&graph, options);
                #[cfg(feature = "skills")]
                let engine = match &own_skills {
                    Some(store) => {
                        #[allow(unused_mut)]
                        let mut own = services_with_skills(&workflow_services, store);
                        #[cfg(feature = "memory")]
                        setup::with_memory_defaults(&mut own, options, &helper);
                        Engine::new(graph).with_services(own)
                    }
                    None => with_workflow_services(Engine::new(graph), &workflow_services, options, &helper),
                };
                #[cfg(not(feature = "skills"))]
                let engine = with_workflow_services(Engine::new(graph), &workflow_services, options, &helper);
                helpers.insert(helper, ServedWorkflow { engine, env });
            }
            carried.insert(name.clone(), helpers);
        }
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
    link_served(&mut workflows, &carried)?;
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
/// A bundle's agents start only the workflows the bundle carries
/// (`carried`, by the bundle's served name), and no other served workflow
/// starts those.
///
/// @throws CliError - when an agent lists a workflow this server does not serve,
///   so a misnamed subagent stops the start, not a run halfway.
#[cfg(feature = "subagents")]
pub(crate) fn link_served(
    workflows: &mut IndexMap<String, ServedWorkflow>,
    carried: &IndexMap<String, IndexMap<String, ServedWorkflow>>,
) -> Result<(), CliError> {
    let served = Linkable::of(workflows);
    let bundles: IndexMap<&String, Linkable> =
        carried.iter().map(|(name, helpers)| (name, Linkable::of(helpers))).collect();
    for (name, workflow) in workflows.iter_mut() {
        let own = crate::setup::listed(workflow.engine.graph());
        if own.is_empty() {
            continue;
        }
        let (pool, what) = match bundles.get(name) {
            Some(bundle) => (bundle, "which its bundle does not carry"),
            None => (&served, "which this server does not serve"),
        };
        let linked = pool.reach(name, own, what)?;
        let graph = Arc::clone(workflow.engine.graph());
        let engine = std::mem::replace(&mut workflow.engine, Engine::new(graph));
        workflow.engine = engine.with_linked(linked);
    }
    Ok(())
}

/// Workflows an agent may start, by name, with what each lists in turn.
#[cfg(feature = "subagents")]
struct Linkable {
    lists: IndexMap<String, Vec<String>>,
    links: IndexMap<String, graphrt_core::engine::LinkedWorkflow>,
}

#[cfg(feature = "subagents")]
impl Linkable {
    fn of(workflows: &IndexMap<String, ServedWorkflow>) -> Self {
        let lists = workflows
            .iter()
            .map(|(name, served)| (name.clone(), crate::setup::listed(served.engine.graph())))
            .collect();
        let links = workflows
            .iter()
            .map(|(name, served)| {
                let link = graphrt_core::engine::LinkedWorkflow::new(Arc::clone(served.engine.graph()))
                    .with_env(served.env.clone())
                    .with_services(Arc::clone(served.engine.services()));
                (name.clone(), link)
            })
            .collect();
        Self { lists, links }
    }

    /// Every workflow reachable from `own` (what workflow `name` lists),
    /// each checked: it exists here and can run as a subagent.
    ///
    /// @throws CliError - for a listed workflow that is not here (`what` says
    ///   where it was looked for) or cannot run as one.
    fn reach(
        &self,
        name: &str,
        own: Vec<String>,
        what: &str,
    ) -> Result<Vec<(String, graphrt_core::engine::LinkedWorkflow)>, CliError> {
        let mut reach: Vec<String> = Vec::new();
        let mut queue: Vec<(String, String)> = own.into_iter().map(|listed| (name.to_owned(), listed)).collect();
        while let Some((by, next)) = queue.pop() {
            if reach.contains(&next) {
                continue;
            }
            let Some(link) = self.links.get(&next) else {
                let known: Vec<&str> = self.links.keys().map(String::as_str).collect();
                return Err(CliError::usage(format!(
                    "workflow `{by}` lists subagent `{next}`, {what} (there: {})",
                    if known.is_empty() { "none".to_owned() } else { known.join(", ") }
                )));
            };
            link.check().map_err(|why| {
                CliError::usage(format!("workflow `{by}` lists subagent `{next}`, which cannot run as one: {why}"))
            })?;
            queue.extend(self.lists.get(&next).cloned().unwrap_or_default().into_iter().map(|n| (next.clone(), n)));
            reach.push(next);
        }
        Ok(reach.into_iter().filter_map(|n| self.links.get(&n).map(|link| (n, link.clone()))).collect())
    }
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
        link_served(&mut workflows, &IndexMap::new()).unwrap();
        let lead = workflows["lead"].engine.fingerprint();
        assert!(lead.contains(":middle=") && lead.contains(":leaf=") && !lead.contains(":other="), "{lead}");
        assert_eq!(workflows["other"].engine.fingerprint(), before_other, "a workflow without subagents is not linked");
        // A listed workflow the server does not serve stops the start.
        let mut workflows = IndexMap::new();
        workflows.insert("lead".to_owned(), served(with_agent(&["researcher"])));
        let error = link_served(&mut workflows, &IndexMap::new()).unwrap_err().to_string();
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
        let error = link_served(&mut workflows, &IndexMap::new()).unwrap_err().to_string();
        assert!(error.contains("cannot run as one") && error.contains("`token`"), "{error}");
        workflows.insert("locked".to_owned(), served(start(serde_json::json!("fallback"))));
        link_served(&mut workflows, &IndexMap::new()).unwrap();
    }
}

#[cfg(all(test, feature = "bundle", feature = "subagents"))]
mod bundle_tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    fn lister(name: &str, listed: &[&str]) -> String {
        let subagents: Vec<serde_json::Value> =
            listed.iter().map(|w| serde_json::json!({"workflow": w, "description": "Helps"})).collect();
        serde_json::json!({"version": "1", "name": name, "graph": {"nodes": [
            {"id": "start", "type": "start"},
            {"id": "lead", "type": "agent", "model": {"provider": "openai", "name": "m"}, "query": "go", "subagents": subagents},
            {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["lead", "text"]}]}],
            "edges": [{"source": "start", "target": "lead"}, {"source": "lead", "target": "end"}]}})
        .to_string()
    }

    #[tokio::test]
    async fn a_served_bundle_links_its_subagents_without_serving_them_under_the_lower_budget() {
        let dir = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-serve-bundle-helpers").unwrap();
        let bytes = graphrt_bundle::BundleBuilder::new("lead", "1.0.0")
            .workflow("workflow.json", lister("lead", &["research"]))
            .unwrap()
            .workflow("workflows/research.json", lister("research", &[]))
            .unwrap()
            .limits(graphrt_bundle::BundleLimits { max_total_tokens: Some(9_000), max_cost_micros: Some(400_000) })
            .build()
            .unwrap();
        let path = dir.join("lead.grt");
        std::fs::write(&path, bytes).unwrap();
        let options = ServiceOptions {
            plugins: crate::extensions::PluginOptions { no_default: true, ..Default::default() },
            ..ServiceOptions::default()
        };
        // The host's lower token budget wins, the bundle's lower cost budget wins too.
        let host = Limits { max_total_tokens: Some(5_000), max_cost_micros: Some(900_000), ..Limits::default() };
        // A served workflow of the same name as the bundle's subagent: neither one replaces the other.
        let research = dir.join("research.json");
        std::fs::write(&research, lister("research", &[])).unwrap();
        let files = [("lead".to_owned(), path), ("research".to_owned(), research)];
        let state = build_state(&files, &options, host, None).await.unwrap();
        let lead = &state.workflows["lead"];
        assert_eq!(lead.engine.limits().max_total_tokens, Some(5_000));
        assert_eq!(lead.engine.limits().max_cost_micros, Some(400_000));
        let linked: Vec<&str> = lead.engine.linked().map(|(name, _)| name).collect();
        assert_eq!(linked, ["research"]);
        let carried = lead.engine.linked().next().unwrap().1.graph();
        assert!(
            !Arc::ptr_eq(carried, state.workflows["research"].engine.graph()),
            "the bundle's own, not the served one"
        );
        assert_eq!(state.workflows.len(), 2, "a bundle adds no endpoint besides its entry");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
