//! `graphrt eval`: run a workflow against the test cases of an eval file.
//!
//! The workflow is loaded and compiled once; every test runs in-process on the
//! same engine (and host services), up to `-j` at a time. Judge workflows
//! (`type: judge`) are compiled once each on the same services and run inside
//! their test, so they share the `-j` limit. Results stream to stderr as tests
//! finish, followed by the summary and the gate verdict.

use std::path::Path;
use std::sync::Arc;

use graphrt_core::engine::Limits;
use graphrt_core::node::TokioClock;
use graphrt_core::{Engine, Registry, Services};
use graphrt_eval::baseline::Baseline;
use graphrt_eval::report::{format_result_line, format_summary};
use graphrt_eval::{
    AssertionRegistry, EngineTarget, EvalError, EvalReport, EvalSuite, JudgeLoader, Runner, RunnerOptions, Target,
};

use crate::args::EvalArgs;
use crate::error::{CliError, EXIT_OK};
use crate::extensions::Extensions;
use crate::setup::{self, ServiceOptions};

/// Execute `graphrt eval`. Returns the process exit code.
pub async fn eval(args: EvalArgs) -> Result<u8, CliError> {
    if args.pass_rate.is_some_and(|r| !(0.0..=1.0).contains(&r)) {
        return Err(CliError::usage("`--pass-rate` must be between 0 and 1"));
    }
    if let Some(raw) = &args.judge_model {
        setup::parse_model_override(raw)?;
    }
    let suite = EvalSuite::load(&args.file).map_err(eval_error)?;
    // Read the baseline first: a wrong path should not cost a whole evaluation.
    let baseline = args.baseline.as_deref().map(Baseline::load).transpose().map_err(eval_error)?;
    let vars = crate::run::service_options(&args.services)?.all_vars();
    let telemetry = crate::otel::Telemetry::from_args(&args.services, &vars)?;
    // `_extensions` keeps plugin hosts (child processes) alive while tests run.
    let (target, judges, _extensions) = build_target(&suite, &args, telemetry.as_ref()).await?;
    #[cfg(feature = "otel")]
    let traced = traced_target(&target, &suite, telemetry.as_ref());
    #[cfg(feature = "otel")]
    let target: &dyn Target = match &traced {
        Some(traced) => traced,
        None => &target,
    };
    #[cfg(not(feature = "otel"))]
    let target: &dyn Target = &target;
    let mut registry = AssertionRegistry::with_builtins();
    let base = args.file.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    graphrt_eval::judge::register(&mut registry, base, judges);
    let mut options = RunnerOptions::default();
    options.concurrency = args.concurrency.max(1);
    options.trials = args.trials.max(1);
    options.filters.clone_from(&args.filter);
    options.pass_rate = args.pass_rate;
    options.keep_events = args.include_events;
    eprintln!(
        "graphrt eval: {} ({} test case(s){}, workflow {}, -j {})",
        args.file.display(),
        suite.cases.len(),
        if options.trials > 1 { format!(" x {} trials", options.trials) } else { String::new() },
        suite.workflow.display(),
        options.concurrency
    );
    let mut report = Runner::new(options)
        .with_registry(registry)
        .run(&suite, target, |result| {
            eprintln!("{}", format_result_line(result));
            #[cfg(feature = "otel")]
            if let Some(traced) = &traced {
                traced.record_result(result);
            }
        })
        .await
        .map_err(eval_error)?;
    if let Some(telemetry) = &telemetry {
        telemetry.shutdown().await;
    }
    if let Some(baseline) = &baseline {
        report.apply_baseline(baseline, &suite.gate.baseline);
    }
    eprint!("{}", format_summary(&report));
    write_outputs(&report, &args)?;
    Ok(if report.gate_passed() { EXIT_OK } else { args.fail_exit_code })
}

/// Load the workflow once and build the engine every test runs on, plus the
/// loader judges compile with. The eval file supplies providers / knowledge /
/// MCP / plugins unless flags override them.
async fn build_target(
    suite: &EvalSuite,
    args: &EvalArgs,
    telemetry: Option<&Arc<crate::otel::Telemetry>>,
) -> Result<(EngineTarget, WorkflowJudges, Extensions), CliError> {
    let mut options = crate::run::service_options(&args.services)?;
    options.capture_content = crate::otel::captures_content(telemetry);
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
    // A calibration suite evaluates a judge: `--judge-model` applies to it too.
    if suite.calibration.is_some() && args.judge_model.is_some() {
        options.model_override.clone_from(&args.judge_model);
    }
    let extensions = Extensions::load(&options.plugins)?;
    let graph = setup::load_graph_with(&suite.workflow, options.model_override.as_deref(), &extensions.registry)?;
    for warning in graph.warnings() {
        eprintln!("warning: {warning}");
    }
    let services = Arc::new(setup::services(&options, extensions.tools.clone()).await?);
    let env = setup::workflow_env(&graph, &options);
    let limits = setup::limits(args.services.timeout_ms, args.services.max_steps);
    let engine = Engine::new(Arc::clone(&graph))
        .with_shared_services(Arc::clone(&services))
        .with_clock(Arc::new(TokioClock))
        .with_limits(limits.clone());
    let target = EngineTarget::new(engine).with_env(env).with_path(suite.workflow.display().to_string());
    let judges = WorkflowJudges {
        registry: extensions.registry.clone(),
        services,
        model: args.judge_model.clone(),
        limits,
        options,
    };
    Ok((target, judges, extensions))
}

/// With telemetry, a target that traces each test run as a Langfuse experiment
/// item ([`crate::eval_trace`]).
#[cfg(feature = "otel")]
fn traced_target(
    target: &EngineTarget,
    suite: &EvalSuite,
    telemetry: Option<&Arc<crate::otel::Telemetry>>,
) -> Option<crate::eval_trace::TracedTarget> {
    let telemetry = telemetry?;
    let name = suite.path.file_stem().map(|s| s.to_string_lossy().trim_end_matches(".eval").to_owned());
    let name = name.unwrap_or_else(|| "eval".to_owned());
    let started =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or_default();
    let experiment = crate::eval_trace::Experiment {
        id: format!("{name}-{started}"),
        name,
        dataset_id: suite.path.display().to_string(),
        description: suite.description.clone(),
    };
    Some(crate::eval_trace::TracedTarget::new(target.clone(), Arc::clone(telemetry), experiment))
}

/// Compiles judge workflows on the suite's (or the server's) services.
/// `--judge-model` (never `--model`) overrides their models.
pub(crate) struct WorkflowJudges {
    pub(crate) registry: Registry,
    pub(crate) services: Arc<Services>,
    pub(crate) model: Option<String>,
    pub(crate) limits: Limits,
    pub(crate) options: ServiceOptions,
}

impl JudgeLoader for WorkflowJudges {
    fn load(&self, path: &Path) -> Result<Arc<dyn Target>, EvalError> {
        let graph = setup::load_graph_with(path, self.model.as_deref(), &self.registry)
            .map_err(|e| EvalError::Invalid(format!("judge workflow {}: {}", path.display(), e.message.trim_end())))?;
        for warning in graph.warnings() {
            eprintln!("warning: judge {}: {warning}", path.display());
        }
        let env = setup::workflow_env(&graph, &self.options);
        let engine = Engine::new(graph)
            .with_shared_services(Arc::clone(&self.services))
            .with_clock(Arc::new(TokioClock))
            .with_limits(self.limits.clone());
        Ok(Arc::new(EngineTarget::new(engine).with_env(env)))
    }
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

/// Write a report owner-only: it holds test inputs and model answers.
fn write_file(path: &Path, text: &str) -> Result<(), CliError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        graphrt_core::private_fs::create_dir_all(dir)
            .map_err(|e| CliError::failed(format!("cannot create `{}`: {e}", dir.display())))?;
    }
    graphrt_core::private_fs::write(path, text)
        .map_err(|e| CliError::failed(format!("cannot write `{}`: {e}", path.display())))
}

pub(crate) fn eval_error(error: EvalError) -> CliError {
    match error {
        EvalError::Invalid(message) | EvalError::Io(message) => CliError::usage(message),
        other => CliError::failed(other.to_string()),
    }
}
