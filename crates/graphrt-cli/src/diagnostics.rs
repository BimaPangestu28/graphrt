//! Rendering workflow validation diagnostics, rustc style or as JSON.
//!
//! ```text
//! error[unknown-field]: unknown field `templat` for node type `template-transform`
//!   --> hello.yaml:16:7
//!    |
//! 16 |       templat: "Hello, {{ name }}"
//!    |       ^^^^^^^ did you mean `template`?
//!    |
//!    = at: graph.nodes[1].templat
//! ```

use std::fmt::Write as _;
use std::path::Path;

use graphrt_core::validate::{Diagnostic, Severity, Validation};

use crate::error::CliError;

/// Output format of `graphrt validate`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum DiagnosticFormat {
    /// Human-readable, with source snippets.
    #[default]
    Text,
    /// One JSON object (for editors and the builder app).
    Json,
}

/// Render every diagnostic with a source snippet, followed by a summary line
/// when there are errors.
pub fn render_text(file: &str, source: &str, diagnostics: &[Diagnostic]) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let width = diagnostics.iter().filter_map(|d| d.span).map(|s| s.line.to_string().len()).max().unwrap_or(1);
    let pad = " ".repeat(width);
    let mut out = String::new();
    for d in diagnostics {
        let level = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let _ = writeln!(out, "{level}[{}]: {}", d.code, d.message);
        let suggestion = d.suggestion.as_ref().map(|s| format!("did you mean `{s}`?"));
        match d.span {
            Some(span) => {
                let _ = writeln!(out, "{pad}--> {file}:{}:{}", span.line, span.column);
                let text = usize::try_from(span.line).ok().and_then(|l| lines.get(l.saturating_sub(1))).copied();
                if let Some(text) = text {
                    let column = usize::try_from(span.column).unwrap_or(1).saturating_sub(1);
                    let available = text.chars().count().saturating_sub(column).max(1);
                    let len = usize::try_from(span.len).unwrap_or(1).clamp(1, available);
                    let indent: String =
                        text.chars().take(column).map(|c| if c == '\t' { '\t' } else { ' ' }).collect();
                    let _ = writeln!(out, "{pad} |");
                    let _ = writeln!(out, "{:>width$} | {text}", span.line);
                    let label = suggestion.as_deref().map(|s| format!(" {s}")).unwrap_or_default();
                    let _ = writeln!(out, "{pad} | {indent}{}{label}", "^".repeat(len));
                    let _ = writeln!(out, "{pad} |");
                }
            }
            None => {
                let _ = writeln!(out, "{pad}--> {file}");
                if let Some(s) = &suggestion {
                    let _ = writeln!(out, "{pad} = {s}");
                }
            }
        }
        if !d.path.is_root() {
            let _ = writeln!(out, "{pad} = at: {}", d.path);
        }
        if let Some(help) = &d.help {
            let _ = writeln!(out, "{pad} = help: {help}");
        }
        out.push('\n');
    }
    let errors = diagnostics.iter().filter(|d| d.is_error()).count();
    let warnings = diagnostics.len() - errors;
    if errors > 0 {
        let _ = write!(out, "error: `{file}` is invalid: {}", count(errors, "error"));
        if warnings > 0 {
            let _ = write!(out, ", {}", count(warnings, "warning"));
        }
        out.push('\n');
    }
    out
}

fn count(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

/// The JSON report of `graphrt validate --format json`.
pub fn to_json(file: &str, validation: &Validation) -> serde_json::Value {
    let errors = validation.errors().count();
    let mut report = serde_json::json!({
        "file": file,
        "valid": errors == 0,
        "errors": errors,
        "warnings": validation.warnings().count(),
        "diagnostics": validation.diagnostics,
    });
    if let (Some(graph), Some(map)) = (&validation.graph, report.as_object_mut()) {
        map.insert("nodes".into(), graph.nodes().len().into());
        map.insert("edges".into(), graph.edges().len().into());
    }
    report
}

/// Read and validate a workflow file.
pub fn validate_file(path: &Path, registry: &graphrt_core::Registry) -> Result<(String, Validation), CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::usage(format!("cannot read workflow `{}`: {e}", path.display())))?;
    let mut validation = graphrt_core::validate::validate_text(&text, registry);
    for d in &mut validation.diagnostics {
        if d.code == "unknown-node-type" {
            d.help = Some(
                "install the plugin providing it (`graphrt plugin install <dir|git-url>`), point \
                 `--plugins <dir>` at it, or check `graphrt nodes` for available kinds"
                    .to_owned(),
            );
        }
    }
    Ok((text, validation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphrt_core::validate::{DocPath, SourceSpan};

    #[test]
    fn renders_snippet_with_caret_and_suggestion() {
        let source = "graph:\n  nodes:\n    - templat: x\n";
        let mut d = Diagnostic::error(
            "unknown-field",
            DocPath::root().key("graph").key("nodes").index(0).key("templat"),
            "unknown field `templat`",
        )
        .with_suggestion(Some("template".into()));
        d.span = Some(SourceSpan { line: 3, column: 7, len: 7 });
        let text = render_text("w.yaml", source, &[d]);
        let expected = "error[unknown-field]: unknown field `templat`\n --> w.yaml:3:7\n  |\n3 |     - templat: x\n  |       ^^^^^^^ did you mean `template`?\n  |\n  = at: graph.nodes[0].templat\n\nerror: `w.yaml` is invalid: 1 error\n";
        assert_eq!(text, expected);
    }
}
