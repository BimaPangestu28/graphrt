//! `graphrt nodes`: the node catalog of this build.

use std::io::Write;

use graphrt_core::catalog::{Handles, NodeDescriptor};

use crate::args::{NodesArgs, SchemaArgs};
use crate::error::{CliError, EXIT_OK};
use crate::extensions::Extensions;
use crate::setup;

/// Descriptors of every built-in node kind compiled into this binary, sorted by kind.
pub fn catalog() -> Vec<NodeDescriptor> {
    setup::builtin_registry().descriptors()
}

/// The workflow document schema for the built-in node kinds (no plugins): the
/// content of `schema/workflow-v1.schema.json` for the default build.
pub fn builtin_schema() -> serde_json::Value {
    graphrt_core::doc_schema::workflow_schema(&catalog())
}

/// Pretty JSON text of a schema, with a trailing newline.
pub fn schema_text(schema: &serde_json::Value) -> Result<String, CliError> {
    let mut text = serde_json::to_string_pretty(schema).map_err(|e| CliError::failed(e.to_string()))?;
    text.push('\n');
    Ok(text)
}

/// Execute `graphrt schema`.
pub fn schema(args: &SchemaArgs) -> Result<u8, CliError> {
    let extensions = Extensions::load(&args.plugins.options())?;
    let schema = graphrt_core::doc_schema::workflow_schema(&extensions.registry.descriptors());
    let text = schema_text(&schema)?;
    std::io::stdout().lock().write_all(text.as_bytes()).map_err(|e| CliError::failed(e.to_string()))?;
    Ok(EXIT_OK)
}

/// Execute `graphrt nodes`.
pub fn nodes(args: &NodesArgs) -> Result<u8, CliError> {
    let extensions = Extensions::load(&args.plugins.options())?;
    let registry = &extensions.registry;
    let mut out = std::io::stdout().lock();
    let io = |e: std::io::Error| CliError::failed(e.to_string());

    if let Some(kind) = &args.kind {
        let descriptor = registry
            .descriptor(kind)
            .ok_or_else(|| CliError::usage(format!("unknown node kind `{kind}` (see `graphrt nodes`)")))?;
        if args.json {
            let text = serde_json::to_string_pretty(&descriptor).map_err(|e| CliError::failed(e.to_string()))?;
            writeln!(out, "{text}").map_err(io)?;
        } else {
            write_detail(&mut out, &descriptor).map_err(io)?;
        }
        return Ok(EXIT_OK);
    }

    let all = registry.descriptors();
    if args.json {
        let text = serde_json::to_string_pretty(&all).map_err(|e| CliError::failed(e.to_string()))?;
        writeln!(out, "{text}").map_err(io)?;
        return Ok(EXIT_OK);
    }
    let width = all.iter().map(|d| d.kind.len()).max().unwrap_or(4).max(4);
    writeln!(out, "{:width$}  {:12}  {:14}  TITLE", "KIND", "CATEGORY", "SOURCE").map_err(io)?;
    for d in &all {
        let category =
            serde_json::to_value(&d.category).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        writeln!(out, "{:width$}  {:12}  {:14}  {}", d.kind, category, d.source, d.title).map_err(io)?;
    }
    writeln!(out, "\n{} node kinds. `graphrt nodes --kind <KIND>` shows outputs and config.", all.len()).map_err(io)?;
    Ok(EXIT_OK)
}

fn write_detail(out: &mut impl Write, d: &NodeDescriptor) -> std::io::Result<()> {
    writeln!(out, "{} ({})", d.title, d.kind)?;
    if !d.description.is_empty() {
        writeln!(out, "\n{}", d.description)?;
    }
    writeln!(out)?;
    if d.container {
        writeln!(out, "container: yes (owns a child scope)")?;
    }
    match &d.handles {
        Handles::Single => writeln!(out, "handles: source")?,
        Handles::Fixed { handles } => writeln!(out, "handles: {}", handles.join(", "))?,
        Handles::Dynamic { config_path } => {
            writeln!(out, "handles: dynamic ({})", config_path.as_deref().unwrap_or("from config"))?;
        }
        _ => writeln!(out, "handles: custom")?,
    }
    writeln!(out, "outputs:")?;
    for o in &d.outputs {
        let ty = serde_json::to_value(o.ty).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        let streaming = if o.streaming { " (streaming)" } else { "" };
        let desc = if o.description.is_empty() { String::new() } else { format!(" - {}", o.description) };
        writeln!(out, "  {}: {ty}{streaming}{desc}", o.name)?;
    }
    if d.dynamic_outputs {
        writeln!(out, "  ... plus outputs defined by the node config")?;
    }
    if d.outputs.is_empty() && !d.dynamic_outputs {
        writeln!(out, "  (none)")?;
    }
    if let Some(props) = d.config_schema.get("properties").and_then(serde_json::Value::as_object) {
        let required: Vec<&str> = d
            .config_schema
            .get("required")
            .and_then(serde_json::Value::as_array)
            .map(|r| r.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        writeln!(out, "config:")?;
        for key in props.keys() {
            let mark = if required.contains(&key.as_str()) { " (required)" } else { "" };
            writeln!(out, "  {key}{mark}")?;
        }
    }
    writeln!(out, "\nFull JSON Schema: graphrt nodes --kind {} --json", d.kind)
}
