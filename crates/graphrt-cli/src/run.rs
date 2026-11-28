//! `graphrt run`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use graphrt_core::engine::{ResumeFrom, RunInput};
use graphrt_core::event::PauseReason;
use graphrt_core::node::TokioClock;
use graphrt_core::response::ResponseAssembler;
use graphrt_core::{Engine, Event, Selector, Snapshot, Value};
use indexmap::IndexMap;

use crate::args::{OutputMode, RunArgs, ServiceArgs};
use crate::error::{CliError, EXIT_ABORTED, EXIT_FAILED, EXIT_OK, EXIT_PAUSED};
use crate::extensions::Extensions;
use crate::setup::{self, ServiceOptions};

/// Build [`ServiceOptions`] from flags, reading the env file if given.
pub fn service_options(args: &ServiceArgs) -> Result<ServiceOptions, CliError> {
    let env_file = match &args.env_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| CliError::usage(format!("cannot read env file `{}`: {e}", path.display())))?;
            setup::parse_env_file(&text)
        }
        None => Vec::new(),
    };
    let providers = args.providers.clone().or_else(|| {
        env_file
            .iter()
            .rev()
            .find(|(k, _)| k == setup::PROVIDERS_ENV)
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var(setup::PROVIDERS_ENV).ok())
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    });
    Ok(ServiceOptions {
        env_file,
        allow_private_network: args.allow_private_network,
        mcp_config: args.mcp.clone(),
        providers,
        allow_python_subprocess: args.allow_python_subprocess,
        model_override: args.model.clone(),
        knowledge: args.knowledge.clone(),
        files_dir: args.files_dir.clone(),
        #[cfg(feature = "files")]
        workspace: args.workspace.clone().map(|dir| setup::WorkspaceOptions {
            dir,
            read_only: args.workspace_read_only,
            per_user: args.workspace_per_user,
            #[cfg(feature = "image")]
            image_tools: args.image_tools,
            #[cfg(feature = "image")]
            image_model: args.image_model.clone(),
            #[cfg(feature = "documents")]
            document_tools: args.document_tools,
        }),
        plugins: crate::extensions::PluginOptions {
            allow_private_network: args.allow_private_network,
            ..args.plugins.options()
        },
        trust_keys: args.trust_key.clone(),
        capture_content: false,
        skills_dir: None,
        learning_dir: None,
        #[cfg(feature = "memory")]
        memory_dir: args.memory_dir.clone(),
        #[cfg(feature = "memory")]
        memory_tenant: match &args.memory_tenant {
            Some(tenant) => {
                graphrt_memory::check_name("tenant", tenant)
                    .map_err(|e| CliError::usage(format!("--memory-tenant: {e}")))?;
                Some(tenant.clone())
            }
            None => None,
        },

        #[cfg(feature = "memory")]
        memory_embedder: args.memory_embedder.clone(),
    })
}

/// Execute `graphrt run`. Returns the process exit code.
pub async fn run(args: RunArgs) -> Result<u8, CliError> {
    let mut options = service_options(&args.services)?;
    let mut extensions = Extensions::load(&options.plugins)?;
    // A bundle brings the workflows of its subagents and the budgets of its runs.
    #[cfg(feature = "bundle")]
    let mut bundled: Option<crate::bundle_cmd::BundleParts> = None;
    let graph = if crate::bundle_cmd::is_bundle(&args.workflow) {
        let trust = &args.services.trust_key;
        #[cfg(feature = "bundle")]
        {
            let parts = crate::bundle_cmd::bundle_parts(&args.workflow, &mut options, &mut extensions.registry, trust)?;
            let entry = Arc::clone(&parts.entry);
            bundled = Some(parts);
            entry
        }
        #[cfg(not(feature = "bundle"))]
        crate::bundle_cmd::bundle_graph(&args.workflow, &mut options, &mut extensions.registry, trust)?
    } else {
        setup::load_graph_with(&args.workflow, options.model_override.as_deref(), &extensions.registry)?
    };
    for warning in graph.warnings() {
        eprintln!("warning: {warning}");
    }

    let mut input = RunInput::new();
    input.inputs = collect_inputs(&args)?;
    for raw in &args.sys {
        let (k, v) = setup::parse_assignment(raw)?;
        input.sys.insert(k, v);
    }
    input.env = setup::workflow_env(&graph, &options);
    if let Some(path) = &args.resume {
        input.resume = Some(load_resume(path, &args.resume_input)?);
    } else if !args.resume_input.is_empty() {
        return Err(CliError::usage("`--resume-input` requires `--resume <snapshot>`"));
    }

    let telemetry = crate::otel::Telemetry::from_args(&args.services, &options.all_vars())?;
    options.capture_content = crate::otel::captures_content(telemetry.as_ref());
    #[allow(unused_mut)]
    let mut services = setup::services(&options, extensions.tools.clone()).await?;
    #[cfg(feature = "memory")]
    {
        let stem = args.workflow.file_stem().and_then(|s| s.to_str()).unwrap_or("workflow").to_owned();
        let name = graph.workflow().name.clone().unwrap_or(stem);
        setup::with_memory_defaults(&mut services, &options, &name);
    }
    #[allow(unused_mut)]
    let mut engine = Engine::new(graph.clone());
    #[cfg(all(feature = "subagents", feature = "bundle"))]
    let carried = bundled.as_ref().map(|parts| parts.helpers.clone()).unwrap_or_default();
    #[cfg(all(feature = "subagents", not(feature = "bundle")))]
    let carried: Vec<(String, Arc<graphrt_core::Graph>)> = Vec::new();
    #[cfg(feature = "subagents")]
    let from_bundle = !carried.is_empty();
    #[cfg(feature = "subagents")]
    if args.workflows.is_some() || from_bundle {
        let mut linked = indexmap::IndexMap::new();
        // The bundle's own workflows, then the files of `--workflows`.
        let mut found: Vec<(String, Arc<graphrt_core::Graph>)> = carried;
        if let Some(dir) = &args.workflows {
            for (name, path) in workflow_files(dir)? {
                found.push((
                    name,
                    setup::load_graph_with(&path, options.model_override.as_deref(), &extensions.registry)?,
                ));
            }
        }
        for (name, graph) in found {
            let env = setup::workflow_env(&graph, &options);
            // The run's services with the child's own memory scope (its workflow's name).
            #[allow(unused_mut)]
            let mut own = services.clone();
            // Scoped by the name it is linked under, as `graphrt serve` does.
            #[cfg(feature = "memory")]
            setup::with_memory_defaults(&mut own, &options, &name);
            let link = graphrt_core::engine::LinkedWorkflow::new(graph).with_env(env).with_services(Arc::new(own));
            if linked.insert(name.clone(), link).is_some() {
                return Err(CliError::usage(format!("--workflows: two workflows are named `{name}`")));
            }
        }
        // Only what the run can reach is linked: every linked workflow is in the
        // fingerprint, and a change to an unrelated file must not refuse a resume.
        let reached = check_listed(&graph, &linked)?;
        linked.retain(|name, _| reached.contains(name));
        engine = engine.with_linked(linked);
    }
    #[cfg(feature = "subagents")]
    if args.workflows.is_none()
        && !from_bundle
        && let Some(name) = setup::listed(&graph).first()
    {
        return Err(CliError::usage(format!(
            "an agent lists subagent `{name}`: pass the folder of its workflow with `--workflows DIR`"
        )));
    }
    #[allow(unused_mut)]
    let mut limits = setup::limits(&args.services);
    #[cfg(feature = "bundle")]
    if let Some(parts) = &bundled {
        limits = parts.budgets.over(&limits);
    }
    let engine = engine.with_services(services).with_clock(Arc::new(TokioClock)).with_limits(limits);

    let mut assembler = ResponseAssembler::new(&graph);
    if let Some(resume) = &input.resume {
        // Nodes that finished before the pause will not re-run; seed their values.
        assembler = assembler.resume_from(&resume.snapshot.root);
    }
    for (k, v) in &input.sys {
        assembler = assembler.with_value(&Selector::of("sys", k), v.clone());
    }

    let context = run_context(&args)?;
    let mut tracer = crate::otel::tracer_for(telemetry.as_ref(), &graph, None, &context);
    let mut stream = engine.run(input);
    #[cfg(feature = "telemetry")]
    crate::telemetry::count_run();
    let handle = stream.handle();
    let interrupt = graphrt_core::task::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\ninterrupted, aborting run...");
            handle.abort("interrupted by user");
        }
    });

    let snapshot_path = args.snapshot_out.clone().unwrap_or_else(|| default_snapshot_path(&args.workflow));
    let mut printer = Printer::new(args.output);
    let mut code = EXIT_FAILED;
    let mut chunks = Vec::new();
    while let Some(event) = stream.next().await {
        tracer.on_event(&event);
        if args.output == OutputMode::Text {
            assembler.push_into(&event, &mut chunks);
            for chunk in chunks.drain(..) {
                printer.text(&chunk.text)?;
            }
        }
        if let Some(c) = printer.event(&event, &snapshot_path)? {
            code = c;
        }
    }
    interrupt.abort();
    tracer.finish();
    if let Some(telemetry) = &telemetry {
        telemetry.shutdown().await;
    }
    Ok(code)
}

/// `--session-id`, `--user-id`, `--tag` and `--metadata`.
fn run_context(args: &RunArgs) -> Result<crate::otel::RunContext, CliError> {
    let mut metadata = IndexMap::new();
    for raw in &args.metadata {
        let (key, value) = raw
            .split_once('=')
            .ok_or_else(|| CliError::usage(format!("expected `--metadata key=value`, got `{raw}`")))?;
        if key.is_empty() {
            return Err(CliError::usage(format!("missing key in `--metadata {raw}`")));
        }
        metadata.insert(key.to_owned(), value.to_owned());
    }
    Ok(crate::otel::RunContext {
        session_id: args.session_id.clone(),
        user_id: args.user_id.clone(),
        tags: args.tags.clone(),
        metadata,
    })
}

fn collect_inputs(args: &RunArgs) -> Result<IndexMap<String, Value>, CliError> {
    let mut inputs = IndexMap::new();
    if let Some(source) = &args.inputs_json {
        let text = if source == "-" {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).map_err(|e| CliError::usage(format!("reading stdin: {e}")))?;
            buf
        } else {
            std::fs::read_to_string(source).map_err(|e| CliError::usage(format!("cannot read `{source}`: {e}")))?
        };
        let json: serde_json::Value = serde_json::from_slice(text.as_bytes())
            .map_err(|e| CliError::usage(format!("invalid inputs JSON: {e}")))?;
        let serde_json::Value::Object(map) = json else {
            return Err(CliError::usage("inputs JSON must be an object"));
        };
        for (k, v) in map {
            inputs.insert(k, Value::from_json(v));
        }
    }
    for raw in &args.inputs {
        let (k, v) = setup::parse_assignment(raw)?;
        inputs.insert(k, v);
    }
    Ok(inputs)
}

fn load_resume(path: &Path, raw_inputs: &[String]) -> Result<ResumeFrom, CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::usage(format!("cannot read snapshot `{}`: {e}", path.display())))?;
    let snapshot = Snapshot::from_json(&text)
        .map_err(|e| CliError::usage(format!("invalid snapshot `{}`: {e}", path.display())))?;
    let mut inputs = IndexMap::new();
    for raw in raw_inputs {
        let (node, json) =
            raw.split_once('=').ok_or_else(|| CliError::usage(format!("expected `node_id=json`, got `{raw}`")))?;
        let value = match serde_json::from_slice::<serde_json::Value>(json.as_bytes()) {
            Ok(v) => Value::from_json(v),
            Err(_) => Value::from(json),
        };
        inputs.insert(node.to_owned(), value);
    }
    Ok(ResumeFrom::new(snapshot, inputs))
}

/// `<dir>/<stem>.snapshot.json` next to the workflow.
pub fn default_snapshot_path(workflow: &Path) -> PathBuf {
    let stem = workflow.file_stem().and_then(|s| s.to_str()).unwrap_or("workflow");
    workflow.with_file_name(format!("{stem}.snapshot.json"))
}

/// Writes run output to stdout / stderr according to the output mode.
struct Printer {
    mode: OutputMode,
    printed_text: bool,
    run_id: String,
}

impl Printer {
    fn new(mode: OutputMode) -> Self {
        Self { mode, printed_text: false, run_id: String::new() }
    }

    fn text(&mut self, text: &str) -> Result<(), CliError> {
        if text.is_empty() {
            return Ok(());
        }
        self.printed_text = true;
        let mut out = std::io::stdout().lock();
        out.write_all(text.as_bytes()).and_then(|()| out.flush()).map_err(io_error)
    }

    fn line(&self, line: &str) -> Result<(), CliError> {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}").map_err(io_error)
    }

    /// Handle one event; terminal events return the process exit code.
    fn event(&mut self, event: &Event, snapshot_path: &Path) -> Result<Option<u8>, CliError> {
        if let Event::RunStarted { run_id, .. } = event {
            self.run_id.clone_from(run_id);
        }
        if self.mode == OutputMode::Events {
            self.line(&serde_json::to_string(event).map_err(json_error)?)?;
        }
        match event {
            Event::NodeFailed { node_id, error, handled: true, .. } if self.mode == OutputMode::Text => {
                eprintln!("warning: node `{node_id}` failed and was handled: {error}");
                Ok(None)
            }
            Event::RunSucceeded { outputs, usage, handled_errors, elapsed_ms } => {
                match self.mode {
                    OutputMode::Text => {
                        if self.printed_text {
                            self.line("")?;
                        } else if !outputs.is_empty() {
                            self.line(&pretty(&outputs_json(outputs))?)?;
                        }
                        let partial = if *handled_errors > 0 {
                            format!(", {handled_errors} handled error(s)")
                        } else {
                            String::new()
                        };
                        eprintln!("done in {elapsed_ms} ms, {} tokens{partial}", usage.total_tokens);
                    }
                    OutputMode::Json => {
                        let result = serde_json::json!({
                            "run_id": self.run_id,
                            "status": "succeeded",
                            "outputs": outputs_json(outputs),
                            "usage": usage,
                            "handled_errors": handled_errors,
                            "elapsed_ms": elapsed_ms,
                        });
                        self.line(&pretty(&result)?)?;
                    }
                    OutputMode::Events => {}
                }
                Ok(Some(EXIT_OK))
            }
            Event::RunFailed { error, error_type, usage, elapsed_ms } => {
                match self.mode {
                    OutputMode::Json => {
                        let result = serde_json::json!({
                            "run_id": self.run_id, "status": "failed", "error": error, "error_type": error_type,
                            "usage": usage, "elapsed_ms": elapsed_ms,
                        });
                        self.line(&pretty(&result)?)?;
                    }
                    _ => {
                        if self.printed_text {
                            self.line("")?;
                        }
                        eprintln!("error: run failed: {error}");
                    }
                }
                Ok(Some(EXIT_FAILED))
            }
            Event::RunAborted { reason, .. } => {
                if self.mode == OutputMode::Json {
                    let result = serde_json::json!({"run_id": self.run_id, "status": "aborted", "reason": reason});
                    self.line(&pretty(&result)?)?;
                } else {
                    eprintln!("run aborted: {reason}");
                }
                Ok(Some(EXIT_ABORTED))
            }
            Event::RunPaused { reasons, snapshot } => {
                let json = snapshot.to_json().map_err(json_error)?;
                std::fs::write(snapshot_path, json).map_err(|e| {
                    CliError::failed(format!("cannot write snapshot `{}`: {e}", snapshot_path.display()))
                })?;
                match self.mode {
                    OutputMode::Json => {
                        let result = serde_json::json!({
                            "run_id": self.run_id, "status": "paused",
                            "reasons": reasons, "snapshot": snapshot_path.display().to_string(),
                        });
                        self.line(&pretty(&result)?)?;
                    }
                    _ => {
                        if self.printed_text {
                            self.line("")?;
                        }
                        print_pause_help(reasons, snapshot_path);
                    }
                }
                Ok(Some(EXIT_PAUSED))
            }
            _ => Ok(None),
        }
    }
}

fn print_pause_help(reasons: &[PauseReason], snapshot_path: &Path) {
    eprintln!("run paused; snapshot written to {}", snapshot_path.display());
    // A node inside a child run is keyed by its frame too (builds with child
    // runs). Elsewhere the node id alone answers it.
    let key = |reason: &PauseReason| {
        let node = reason.node_id.clone()?;
        #[cfg(feature = "subagents")]
        if !reason.frame.is_empty() {
            return Some(format!("{}/{node}", reason.frame));
        }
        Some(node)
    };
    for reason in reasons {
        let node = key(reason).unwrap_or_else(|| "-".to_owned());
        eprintln!("  waiting: {} (node `{node}`)", reason.kind);
        if !reason.data.is_null() {
            eprintln!("  details: {}", serde_json::to_string(&reason.data).unwrap_or_default());
        }
    }
    let node = reasons.iter().find_map(key).unwrap_or_else(|| "<node>".to_owned());
    eprintln!(
        "resume with: graphrt run <workflow> --resume {} --resume-input '{node}={{\"action\":\"<id>\",\"values\":{{}}}}'",
        snapshot_path.display()
    );
}

fn outputs_json(outputs: &IndexMap<String, Value>) -> serde_json::Value {
    serde_json::Value::Object(outputs.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
}

fn pretty(value: &serde_json::Value) -> Result<String, CliError> {
    serde_json::to_string_pretty(value).map_err(json_error)
}

fn io_error(e: std::io::Error) -> CliError {
    CliError::failed(format!("writing output: {e}"))
}

fn json_error(e: serde_json::Error) -> CliError {
    CliError::failed(format!("encoding JSON: {e}"))
}

/// The subagents the workflow can reach (what it lists, what those list in
/// turn). Each must be a workflow file of the folder that can run as one, so a
/// misnamed one stops the run before it starts. Every file of the folder is
/// loaded, only the reachable ones are checked.
#[cfg(feature = "subagents")]
fn check_listed(
    graph: &graphrt_core::graph::Graph,
    linked: &IndexMap<String, graphrt_core::engine::LinkedWorkflow>,
) -> Result<Vec<String>, CliError> {
    let mut seen: Vec<String> = Vec::new();
    let mut queue: Vec<(String, String)> =
        setup::listed(graph).into_iter().map(|name| ("the workflow".to_owned(), name)).collect();
    while let Some((by, name)) = queue.pop() {
        if seen.contains(&name) {
            continue;
        }
        let Some(link) = linked.get(&name) else {
            return Err(CliError::usage(format!(
                "--workflows: {by} lists subagent `{name}`, but no workflow file there is named `{name}`"
            )));
        };
        link.check()
            .map_err(|why| CliError::usage(format!("--workflows: subagent `{name}` cannot run as one: {why}")))?;
        let lister = format!("`{name}`");
        queue.extend(setup::listed(link.graph()).into_iter().map(|next| (lister.clone(), next)));
        seen.push(name);
    }
    Ok(seen)
}

/// The workflow files of `path` (one file, or each `.yaml`, `.yml` and `.json`
/// of a directory), each with its file name as the workflow's name.
#[cfg(feature = "subagents")]
fn workflow_files(path: &std::path::Path) -> Result<Vec<(String, std::path::PathBuf)>, CliError> {
    let name_of = |p: &std::path::Path| p.file_stem().and_then(|s| s.to_str()).unwrap_or("workflow").to_owned();
    if path.is_file() {
        return Ok(vec![(name_of(path), path.to_path_buf())]);
    }
    let entries = std::fs::read_dir(path)
        .map_err(|e| CliError::usage(format!("--workflows: cannot read `{}`: {e}", path.display())))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| CliError::usage(format!("--workflows: cannot read `{}`: {e}", path.display())))?;
        paths.push(entry.path());
    }
    // Workflow files only: not the snapshots a paused run writes next to them,
    // nor an MCP configuration.
    let workflow = |p: &std::path::Path| {
        p.is_file()
            && matches!(p.extension().and_then(|e| e.to_str()), Some("yaml" | "yml" | "json"))
            && !p.to_string_lossy().ends_with(".snapshot.json")
            && !setup::is_mcp_config(p)
    };
    let mut found: Vec<(String, std::path::PathBuf)> =
        paths.into_iter().filter(|p| workflow(p)).map(|p| (name_of(&p), p)).collect();
    found.sort();
    Ok(found)
}
