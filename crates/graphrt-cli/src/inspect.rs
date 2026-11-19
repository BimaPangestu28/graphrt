//! `graphrt validate` and `graphrt inspect`.

use std::io::Write;
use std::path::Path;

use graphrt_core::{Graph, Registry, Workflow};

use crate::diagnostics::{self, DiagnosticFormat};
use crate::error::{CliError, EXIT_INVALID, EXIT_OK};
use crate::extensions::{Extensions, PluginOptions};
use crate::setup;

/// Parse and compile a workflow, reporting every error and warning. Errors exit with code 2.
pub fn validate(path: &Path, format: DiagnosticFormat, plugins: &PluginOptions) -> Result<u8, CliError> {
    let extensions = Extensions::load(plugins)?;
    let (text, validation) = diagnostics::validate_file(path, &extensions.registry)?;
    let file = path.display().to_string();
    let mut out = std::io::stdout().lock();
    let io = |e: std::io::Error| CliError::failed(e.to_string());
    if format == DiagnosticFormat::Json {
        let report = diagnostics::to_json(&file, &validation);
        let text = serde_json::to_string_pretty(&report).map_err(|e| CliError::failed(e.to_string()))?;
        writeln!(out, "{text}").map_err(io)?;
        return Ok(if validation.has_errors() { EXIT_INVALID } else { EXIT_OK });
    }
    let report = diagnostics::render_text(&file, &text, &validation.diagnostics);
    match &validation.graph {
        Some(graph) => {
            eprint!("{report}");
            writeln!(out, "{file}: ok ({} nodes, {} edges)", graph.nodes().len(), graph.edges().len()).map_err(io)?;
            Ok(EXIT_OK)
        }
        None => Err(CliError::invalid_report(report)),
    }
}

/// Describe a workflow: nodes, scopes, inputs, env variables and missing node kinds.
pub fn inspect(path: &Path, plugins: &PluginOptions) -> Result<u8, CliError> {
    let workflow = setup::read_workflow(path)?;
    let extensions = Extensions::load(plugins)?;
    let report = describe(&workflow, &extensions.registry);
    let mut out = std::io::stdout().lock();
    out.write_all(report.as_bytes()).map_err(|e| CliError::failed(e.to_string()))?;
    Ok(EXIT_OK)
}

/// Human-readable description of a workflow document.
pub fn describe(workflow: &Workflow, registry: &Registry) -> String {
    let mut s = String::new();
    let available: Vec<String> = registry.kinds().map(str::to_owned).collect();
    let line = |s: &mut String, text: String| {
        s.push_str(&text);
        s.push('\n');
    };

    line(&mut s, format!("name:    {}", workflow.name.as_deref().unwrap_or("(unnamed)")));
    line(&mut s, format!("version: {}", workflow.version));
    if let Some(d) = &workflow.description {
        line(&mut s, format!("about:   {d}"));
    }

    line(&mut s, format!("\nnodes ({}):", workflow.graph.nodes.len()));
    let mut missing = Vec::new();
    for node in &workflow.graph.nodes {
        let scope = node.parent.as_deref().map(|p| format!("  [in {p}]")).unwrap_or_default();
        let flag = if available.contains(&node.kind) {
            ""
        } else {
            if !missing.contains(&node.kind) {
                missing.push(node.kind.clone());
            }
            "  (not available: missing plugin or build feature)"
        };
        line(&mut s, format!("  {:<20} {:<22} {}{scope}{flag}", node.id, node.kind, node.label()));
    }
    line(&mut s, format!("\nedges: {}", workflow.graph.edges.len()));

    let starts: Vec<_> = workflow.graph.nodes.iter().filter(|n| n.kind == "start").collect();
    line(&mut s, "\ninputs:".to_owned());
    let mut any_input = false;
    for start in starts {
        if let Some(serde_json::Value::Array(vars)) = start.config.get("variables") {
            for var in vars {
                let name = var.get("variable").and_then(|v| v.as_str()).unwrap_or("?");
                let kind = var.get("type").and_then(|v| v.as_str()).unwrap_or("text-input");
                let required = var.get("required").and_then(serde_json::Value::as_bool).unwrap_or(false);
                let req = if required { "required" } else { "optional" };
                line(&mut s, format!("  {name:<20} {kind:<12} {req}"));
                any_input = true;
            }
        }
    }
    if !any_input {
        line(&mut s, "  (none)".to_owned());
    }

    line(&mut s, "\nenv:".to_owned());
    if workflow.env.is_empty() {
        line(&mut s, "  (none)".to_owned());
    }
    for decl in &workflow.env {
        let source = if decl.value.is_some() { "has default" } else { "must be provided" };
        line(&mut s, format!("  {:<20} {:<12} {source}", decl.name, format!("{:?}", decl.ty).to_lowercase()));
    }

    if !workflow.conversation.is_empty() {
        line(&mut s, "\nconversation variables:".to_owned());
        for decl in &workflow.conversation {
            line(&mut s, format!("  {:<20} {}", decl.name, format!("{:?}", decl.ty).to_lowercase()));
        }
    }

    if missing.is_empty() {
        match Graph::build(workflow.clone(), registry) {
            Ok(graph) => {
                line(&mut s, "\nstatus: valid".to_owned());
                for w in graph.warnings() {
                    line(&mut s, format!("  warning: {w}"));
                }
            }
            Err(e) => line(&mut s, format!("\nstatus: invalid: {e}")),
        }
    } else {
        line(
            &mut s,
            format!(
                "\nstatus: needs node kinds that are not available: {} \
                 (install the plugin with `graphrt plugin install`, pass `--plugins <dir>`, or use a build with the feature)",
                missing.join(", ")
            ),
        );
    }
    s
}
