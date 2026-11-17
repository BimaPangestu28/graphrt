//! Example plugin with three node kinds:
//!
//! - `slugify`: `text` -> `slug` (URL-friendly lowercase text).
//! - `word-count`: `text` -> `words`, and streams a `report` line by line.
//! - `length-router`: branches to `short` or `long` by comparing the `text` length with `threshold`.

use graphrt_wasm_guest::{
    chunk, export_plugin, json, log, progress, Level, Manifest, NodeSpec, Output, Plugin, PluginError, Request,
};

pub struct TextTools;

impl Plugin for TextTools {
    fn manifest() -> Manifest {
        Manifest::new("text-tools", "0.1.0")
            .describe("Small text utilities")
            .node(
                NodeSpec::new("slugify", "Slugify")
                    .describe("Turn text into a URL-friendly slug")
                    .category("transform")
                    .icon("link")
                    .schema(json!({
                        "type": "object",
                        "required": ["text"],
                        "properties": {
                            "text": {"type": "string"},
                            "separator": {"type": "string", "default": "-"}
                        }
                    }))
                    .output("slug", "string"),
            )
            .node(
                NodeSpec::new("word-count", "Word count")
                    .describe("Count words and stream a per-word report")
                    .category("transform")
                    .schema(json!({"type": "object", "required": ["text"], "properties": {"text": {"type": "string"}}}))
                    .output("words", "integer")
                    .streaming_output("report", "string"),
            )
            .node(
                NodeSpec::new("length-router", "Length router")
                    .describe("Route short and long texts differently")
                    .category("logic")
                    .branches(&["short", "long"])
                    .output("length", "integer"),
            )
    }

    fn run(req: Request) -> Result<Output, PluginError> {
        match req.kind.as_str() {
            "slugify" => {
                let text = req.config_str("text").ok_or_else(|| PluginError::invalid_input("`text` is required"))?;
                let sep = req.config_str("separator").unwrap_or_else(|| "-".into());
                Ok(Output::new().with("slug", json!(slugify(&text, &sep))))
            }
            "word-count" => {
                let text = req.config_str("text").unwrap_or_default();
                let mut report = String::new();
                let mut count = 0u64;
                for word in text.split_whitespace() {
                    count += 1;
                    let line = format!("{count}: {word}\n");
                    chunk("report", &line);
                    report.push_str(&line);
                }
                progress("counted", &json!({"words": count}));
                log(Level::Info, &format!("counted {count} words"));
                Ok(Output::new().with("words", json!(count)).with("report", json!(report)))
            }
            "length-router" => {
                let text = req.config_str("text").unwrap_or_default();
                let threshold = req.config("threshold").and_then(|v| v.as_u64()).unwrap_or(20);
                let len = text.chars().count() as u64;
                let handle = if len <= threshold { "short" } else { "long" };
                Ok(Output::new().with("length", json!(len)).branch(handle))
            }
            other => Err(PluginError::new("unknown_kind", format!("unknown node kind `{other}`"))),
        }
    }
}

/// Lowercase alphanumerics joined by `sep`.
pub fn slugify(text: &str, sep: &str) -> String {
    let mut out = String::new();
    let mut pending = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if pending && !out.is_empty() {
                out.push_str(sep);
            }
            pending = false;
            out.extend(c.to_lowercase());
        } else {
            pending = true;
        }
    }
    out
}

export_plugin!(TextTools);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_works() {
        assert_eq!(slugify("  Hello, World! 2026 ", "-"), "hello-world-2026");
        assert_eq!(slugify("Ünïcode Text", "_"), "ünïcode_text");
    }
}
