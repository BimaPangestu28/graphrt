//! Print the eval file JSON Schema (`schema/eval-v1.schema.json`).
//!
//!     cargo run -q -p graphrt-eval --example eval-schema > schema/eval-v1.schema.json

use std::io::Write;

fn main() -> std::io::Result<()> {
    let schema = graphrt_eval::schema::eval_file_schema();
    let text = serde_json::to_string_pretty(&schema).map_err(std::io::Error::other)?;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{text}")
}
