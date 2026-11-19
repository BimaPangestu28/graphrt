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
        plugins: args.plugins.options(),
        trust_keys: args.trust_key.clone(),
    })
}

/// Execute `graphrt run`. Returns the process exit code.
pub async fn run(args: RunArgs) -> Result<u8, CliError> {
    let mut options = service_options(&args.services)?;
    let mut extensions = Extensions::load(&options.plugins)?;
    let graph = if crate::bundle_cmd::is_bundle(&args.workflow) {
        let trust = &args.services.trust_key;
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

    let services = setup::services(&options, extensions.tools.clone()).await?;
    let engine = Engine::new(graph.clone())
        .with_services(services)
        .with_clock(Arc::new(TokioClock))
        .with_limits(setup::limits(args.services.timeout_ms, args.services.max_steps));

    let mut assembler = ResponseAssembler::new(&graph);
    if let Some(resume) = &input.resume {
        // Nodes that finished before the pause will not re-run; seed their values.
        assembler = assembler.resume_from(&resume.snapshot.root);
    }
    for (k, v) in &input.sys {
        assembler = assembler.with_value(&Selector::of("sys", k), v.clone());
    }

    let telemetry = crate::otel::Telemetry::from_args(&args.services, &options.all_vars())?;
    let mut tracer = crate::otel::tracer_for(telemetry.as_ref(), &graph, None);
    let mut stream = engine.run(input);
    let handle = stream.handle();
    let interrupt = tokio::spawn(async move {
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
        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| CliError::usage(format!("invalid inputs JSON: {e}")))?;
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
        let value = match serde_json::from_str::<serde_json::Value>(json) {
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
    for reason in reasons {
        let node = reason.node_id.as_deref().unwrap_or("-");
        eprintln!("  waiting: {} (node `{node}`)", reason.kind);
        if !reason.data.is_null() {
            eprintln!("  details: {}", serde_json::to_string(&reason.data).unwrap_or_default());
        }
    }
    let node = reasons.iter().find_map(|r| r.node_id.clone()).unwrap_or_else(|| "<node>".to_owned());
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
