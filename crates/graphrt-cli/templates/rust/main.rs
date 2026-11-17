//! graphrt plugin `{{name}}`.
//!
//! Build with `cargo build --release`, then run a workflow with
//! `graphrt run workflow.yaml --plugins <parent dir>`.

use graphrt_plugin_sdk::{Plugin, PluginError, RunResult};

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    Plugin::new("{{name}}")
        .node("{{name}}.greet", |ctx, req| async move {
            let name = req
                .config
                .get("name")
                .and_then(|n| n.as_str())
                .filter(|n| !n.is_empty())
                .ok_or_else(|| PluginError::invalid_input("`name` must be a non-empty string"))?
                .to_owned();
            let greeting = format!("Hello, {name}!");
            // Stream word by word; the chunks concatenate to exactly `greeting`.
            for (i, word) in greeting.split(' ').enumerate() {
                let piece = if i == 0 { word.to_owned() } else { format!(" {word}") };
                ctx.chunk("text", &piece);
            }
            Ok(RunResult::new().output("length", greeting.len()).output("text", greeting))
        })
        .serve_stdio()
        .await
}
