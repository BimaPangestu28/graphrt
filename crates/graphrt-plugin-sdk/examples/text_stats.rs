//! Example plugin: text statistics.
//!
//! Build: `cargo build --release -p graphrt-plugin-sdk --example text_stats`
//! Manifest: `plugins/examples/rust-text-stats/graphrt-plugin.json`.

use graphrt_plugin_sdk::{Plugin, PluginError, RunResult, ToolResult};
use serde_json::json;

struct Stats {
    characters: usize,
    words: usize,
    sentences: usize,
    reading_seconds: f64,
}

fn stats(text: &str) -> Stats {
    let words = text.split_whitespace().count();
    let sentences = text.split(['.', '!', '?']).filter(|s| !s.trim().is_empty()).count();
    #[allow(clippy::cast_precision_loss)]
    let reading_seconds = (words as f64 / 3.5).ceil();
    Stats { characters: text.chars().count(), words, sentences, reading_seconds }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    Plugin::new("textstats")
        .node("textstats.analyze", |ctx, req| async move {
            let text = req
                .config
                .get("text")
                .and_then(|t| t.as_str())
                .ok_or_else(|| PluginError::invalid_input("`text` must be a string"))?
                .to_owned();
            ctx.progress("analyzing", json!({"bytes": text.len()}));
            let s = stats(&text);
            Ok(RunResult::new()
                .output("characters", s.characters)
                .output("words", s.words)
                .output("sentences", s.sentences)
                .output("reading_seconds", s.reading_seconds)
                .branch(if s.words > 100 { "long" } else { "short" }))
        })
        .tool(
            json!({
                "name": "word_count",
                "description": "Count the words in a text",
                "parameters": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
            }),
            |call| async move {
                let text = call.arguments.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                Ok(ToolResult::text(stats(text).words.to_string()))
            },
        )
        .serve_stdio()
        .await
}
