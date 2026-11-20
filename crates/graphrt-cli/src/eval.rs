//! `graphrt eval`: run a workflow against the test cases of an eval file.
//!
//! The workflow is loaded and compiled once; every test runs in-process on the
//! same engine (and host services), up to `-j` at a time. Results stream to
//! stderr as tests finish, followed by the summary and the gate verdict.

use std::path::Path;
use std::sync::Arc;

use graphrt_core::Engine;
use graphrt_core::node::TokioClock;
use graphrt_eval::report::{format_result_line, format_summary};
use graphrt_eval::{EngineTarget, EvalError, EvalReport, EvalSuite, Runner, RunnerOptions};

use crate::args::EvalArgs;
use crate::error::{CliError, EXIT_OK};
use crate::extensions::Extensions;
use crate::setup;

/// Execute `graphrt eval`. Returns the process exit code.
pub async fn eval(args: EvalArgs) -> Result<u8, CliError> {
    if args.pass_rate.is_some_and(|r| !(0.0..=1.0).contains(&r)) {
        return Err(CliError::usage("`--pass-rate` must be between 0 and 1"));
    }
    let suite = EvalSuite::load(&args.file).map_err(eval_error)?;
    if args.services.otel || args.services.otel_stdout {
        eprintln!("warning: `graphrt eval` does not export OpenTelemetry traces yet; --otel is ignored");
    }
    // `_extensions` keeps plugin hosts (child processes) alive while tests run.
    let (target, _extensions) = build_target(&suite, &args).await?;
    let mut options = RunnerOptions::default();
    options.concurrency = args.concurrency.max(1);
    options.repeat = args.repeat.max(1);
    options.filters.clone_from(&args.filter);
    options.pass_rate = args.pass_rate;
    options.keep_events = args.include_events;
    eprintln!(
        "graphrt eval: {} ({} test case(s), workflow {}, -j {})",
        args.file.display(),
        suite.cases.len(),
        suite.workflow.display(),
        options.concurrency
    );
    let report = Runner::new(options)
        .run(&suite, &target, |result| eprintln!("{}", format_result_line(result)))
        .await
        .map_err(eval_error)?;
    eprint!("{}", format_summary(&report));
    write_outputs(&report, &args)?;
    Ok(if report.gate_passed() { EXIT_OK } else { args.fail_exit_code })
}

/// Load the workflow once and build the engine every test runs on. The eval
/// file supplies providers / knowledge / MCP / plugins unless flags override them.
async fn build_target(suite: &EvalSuite, args: &EvalArgs) -> Result<(EngineTarget, Extensions), CliError> {
    let mut options = crate::run::service_options(&args.services)?;
    if args.services.providers.is_none() && suite.providers.is_some() {
        options.providers.clone_from(&suite.providers);
    }
    if options.knowledge.is_none() {
        options.knowledge.clone_from(&suite.knowledge);
    }
    if options.mcp_config.is_none() {
        options.mcp_config.clone_from(&suite.mcp);
    }
    options.plugins.dirs.extend(suite.plugins.iter().cloned());
    let extensions = Extensions::load(&options.plugins)?;
    let graph = setup::load_graph_with(&suite.workflow, options.model_override.as_deref(), &extensions.registry)?;
    for warning in graph.warnings() {
        eprintln!("warning: {warning}");
    }
    let services = setup::services(&options, extensions.tools.clone()).await?;
    let env = setup::workflow_env(&graph, &options);
    let engine = Engine::new(Arc::clone(&graph))
        .with_services(services)
        .with_clock(Arc::new(TokioClock))
        .with_limits(setup::limits(args.services.timeout_ms, args.services.max_steps));
    let target = EngineTarget::new(engine).with_env(env).with_path(suite.workflow.display().to_string());
    Ok((target, extensions))
}

fn write_outputs(report: &EvalReport, args: &EvalArgs) -> Result<(), CliError> {
    if let Some(path) = &args.output {
        let json = report.to_json().map_err(|e| CliError::failed(format!("encoding results: {e}")))?;
        write_file(path, &json)?;
        eprintln!("results: {}", path.display());
    }
    if let Some(path) = &args.junit {
        write_file(path, &report.to_junit())?;
        eprintln!("junit: {}", path.display());
    }
    Ok(())
}

fn write_file(path: &Path, text: &str) -> Result<(), CliError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| CliError::failed(format!("cannot create `{}`: {e}", dir.display())))?;
    }
    std::fs::write(path, text).map_err(|e| CliError::failed(format!("cannot write `{}`: {e}", path.display())))
}

fn eval_error(error: EvalError) -> CliError {
    match error {
        EvalError::Invalid(message) | EvalError::Io(message) => CliError::usage(message),
        other => CliError::failed(other.to_string()),
    }
}
