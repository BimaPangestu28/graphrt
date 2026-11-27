//! `graphrt kb`: ingest documents into, and query, a knowledge base defined by a
//! `--knowledge` config file.

use std::io::Write;
use std::path::Path;

use crate::args::{KbArgs, KbCommand};
use crate::error::{CliError, EXIT_OK};
use crate::setup;

/// Execute a `graphrt kb` subcommand.
pub async fn kb(args: KbArgs) -> Result<u8, CliError> {
    let env_file = match &args.env_file {
        Some(path) => setup::parse_env_file(
            &std::fs::read_to_string(path)
                .map_err(|e| CliError::usage(format!("cannot read env file `{}`: {e}", path.display())))?,
        ),
        None => Vec::new(),
    };
    let options = setup::ServiceOptions { env_file, ..Default::default() };
    let kb = setup::load_knowledge(&args.knowledge, &options.all_vars())?;
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
        #[cfg(feature = "connectors")]
        KbCommand::Sync { source } => return sync(&args.knowledge, &kb, &options.all_vars(), &source, &mut out).await,
    }
    Ok(EXIT_OK)
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

/// `graphrt kb sync`: sync the config's sources once, one after the other.
#[cfg(feature = "connectors")]
async fn sync(
    config: &Path,
    kb: &graphrt_rag::KnowledgeBase,
    vars: &[(String, String)],
    only: &[String],
    out: &mut impl Write,
) -> Result<u8, CliError> {
    use graphrt_connect::SyncOutcome;
    let io = |e: std::io::Error| CliError::failed(e.to_string());
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let scheduler = crate::knowledge_sync::load_scheduler(config, kb, vars, gate)?
        .ok_or_else(|| CliError::usage(format!("knowledge config `{}` declares no sources", config.display())))?;
    let ids: Vec<String> =
        if only.is_empty() { scheduler.sources().into_iter().map(|s| s.id).collect() } else { only.to_vec() };
    let mut failed = 0usize;
    for id in &ids {
        match scheduler.sync_now(id).await {
            Ok(status) => {
                let report = status.report;
                writeln!(
                    out,
                    "{id}: {} indexed, {} unchanged, {} removed, {} failed ({} documents, {} ms)",
                    report.indexed,
                    report.unchanged,
                    report.removed,
                    report.failed,
                    status.document_count,
                    status.duration_ms.unwrap_or(0)
                )
                .map_err(io)?;
                for warning in &status.warnings {
                    writeln!(out, "  warning: {warning}").map_err(io)?;
                }
                if let Some(error) = &status.error {
                    writeln!(out, "  error ({}): {}", error.kind, error.message).map_err(io)?;
                }
                if status.outcome == SyncOutcome::Failed {
                    failed += 1;
                }
            }
            Err(error) => {
                writeln!(out, "{id}: {error}").map_err(io)?;
                failed += 1;
            }
        }
    }
    if failed > 0 {
        return Err(CliError::failed(format!("{failed} of {} sources failed to sync", ids.len())));
    }
    Ok(EXIT_OK)
}
