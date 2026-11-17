//! `graphrt kb`: ingest documents into, and query, a knowledge base defined by a
//! `--knowledge` config file.

use std::io::Write;
use std::path::Path;

use crate::args::{KbArgs, KbCommand};
use crate::error::{CliError, EXIT_OK};
use crate::setup;

/// Execute a `graphrt kb` subcommand.
pub async fn kb(args: KbArgs) -> Result<u8, CliError> {
    let text = std::fs::read_to_string(&args.knowledge)
        .map_err(|e| CliError::usage(format!("cannot read knowledge config `{}`: {e}", args.knowledge.display())))?;
    let value = setup::parse_structured(&text, "knowledge config")?;
    let config = graphrt_rag::KnowledgeConfig::from_json(&value.to_string())
        .map_err(|e| CliError::usage(format!("knowledge config: {e}")))?;
    let env_file = match &args.env_file {
        Some(path) => setup::parse_env_file(
            &std::fs::read_to_string(path)
                .map_err(|e| CliError::usage(format!("cannot read env file `{}`: {e}", path.display())))?,
        ),
        None => Vec::new(),
    };
    let options = setup::ServiceOptions { env_file, ..Default::default() };
    let vars = options.all_vars();
    let lookup = |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let kb = config.build_with_env(&lookup).map_err(|e| CliError::usage(format!("knowledge config: {e}")))?;
    let mut out = std::io::stdout().lock();
    let io = |e: std::io::Error| CliError::failed(e.to_string());

    match args.command {
        KbCommand::Ingest { dataset, replace, files } => {
            let ingestor = kb
                .ingestor(&dataset)
                .ok_or_else(|| CliError::usage(format!("dataset `{dataset}` is unknown or not store-backed")))?
                .replace_existing(replace);
            let extractor = graphrt_rag::DocExtractor::new();
            let (mut stored, mut skipped) = (0usize, 0usize);
            for path in &files {
                let bytes = std::fs::read(path)
                    .map_err(|e| CliError::usage(format!("cannot read `{}`: {e}", path.display())))?;
                let name = file_name(path);
                let doc = graphrt_rag::Document::new(&dataset, path.display().to_string(), String::new()).title(&name);
                let report = ingestor
                    .ingest_bytes(doc, &bytes, None, &name, &extractor)
                    .await
                    .map_err(|e| CliError::failed(format!("{}: {e}", path.display())))?;
                writeln!(out, "{}: {} chunks stored, {} unchanged", path.display(), report.stored, report.skipped)
                    .map_err(io)?;
                stored += report.stored;
                skipped += report.skipped;
            }
            writeln!(out, "done: {} files, {stored} chunks stored, {skipped} unchanged", files.len()).map_err(io)?;
        }
        KbCommand::Query { dataset, query, top_k } => {
            let mut request = graphrt_rag::RetrievalRequest::new(query, dataset);
            request.top_k = top_k;
            let hits = kb.retrieve(&request).await.map_err(|e| CliError::failed(e.to_string()))?;
            let json = serde_json::to_string_pretty(&hits).map_err(|e| CliError::failed(e.to_string()))?;
            writeln!(out, "{json}").map_err(io)?;
        }
    }
    Ok(EXIT_OK)
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}
