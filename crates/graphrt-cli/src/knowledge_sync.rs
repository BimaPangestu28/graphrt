//! The `sources:` of a knowledge config: connectors that keep its datasets in
//! sync (graphrt-connect). `graphrt serve` runs their schedules in the
//! background, `graphrt kb sync` syncs them once.
//!
//! Secrets are named in the config (`token_env: NOTION_TOKEN`) and resolved
//! from the environment and `--env-file`, like every other secret of a
//! knowledge config. The sync state and lock live in one SQLite file next to
//! the config (or at `sync_state`), so two processes that share it never sync
//! the same source at once.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use graphrt_connect::{
    BuildContext, ConnectorRegistry, KnowledgeSink, Scheduler, SourceConfig, SqliteSyncState, SyncOptions,
};
use graphrt_rag::KnowledgeBase;
use tokio::sync::Semaphore;

use crate::error::CliError;

/// Default sync state file next to a knowledge config (`kb.yaml` -> `kb.sync.sqlite`).
pub fn default_state_path(config: &Path) -> PathBuf {
    let stem = config.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "knowledge".into());
    config.with_file_name(format!("{stem}.sync.sqlite"))
}

/// Who holds a sync lock taken by this process.
fn holder() -> String {
    let host = std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).unwrap_or_else(|_| "host".into());
    format!("graphrt-serve:{host}:{}", std::process::id())
}

/// The sources of the knowledge config at `config`, with their connectors,
/// ready to schedule. `None` when the config declares no sources.
///
/// A source whose connector cannot be built (a missing secret, an unknown
/// dataset) is still listed, with the reason, so it shows up in
/// `GET /v1/knowledge/sources` instead of failing startup.
///
/// @throws `CliError::Usage` for an unreadable config, invalid sources or an
/// unusable sync state file.
pub fn load_scheduler(
    config: &Path,
    kb: &KnowledgeBase,
    vars: &[(String, String)],
    gate: Arc<Semaphore>,
) -> Result<Option<Scheduler>, CliError> {
    let text = std::fs::read_to_string(config)
        .map_err(|e| CliError::usage(format!("cannot read knowledge config `{}`: {e}", config.display())))?;
    let value = crate::setup::parse_structured(&text, &format!("knowledge config `{}`", config.display()))?;
    let Some(raw_sources) = value.get("sources").and_then(serde_json::Value::as_array).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let sources = SourceConfig::parse_list(raw_sources)
        .map_err(|e| CliError::usage(format!("knowledge config `{}`: {e}", config.display())))?;
    let state_path = match value.get("sync_state").and_then(serde_json::Value::as_str) {
        Some(path) if Path::new(path).is_absolute() => PathBuf::from(path),
        Some(path) => config.parent().unwrap_or_else(|| Path::new(".")).join(path),
        None => default_state_path(config),
    };
    let state = SqliteSyncState::open(&state_path)
        .map_err(|e| CliError::usage(format!("sync state `{}`: {e}", state_path.display())))?;
    let sink = KnowledgeSink::new(kb.clone()).with_gate(gate);
    let options = SyncOptions { holder: holder(), ..SyncOptions::default() };
    let scheduler = Scheduler::new(Arc::new(sink), Arc::new(state), options);
    let registry = ConnectorRegistry::with_builtins();
    let lookup = |name: &str| vars.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
    let mut ctx = BuildContext::new(&lookup);
    // Rotated refresh tokens stay in memory; the configured one is used again after a restart.
    ctx.on_rotate = Some(Arc::new(|_| {
        eprintln!("knowledge sync: the sign-in server issued a new refresh token; it is used until the next restart");
    }));
    for source in sources {
        let connector = if source.dataset.is_empty() {
            Err(graphrt_connect::ConnectError::Config(format!("source `{}` needs a `dataset`", source.id)))
        } else if !kb.is_managed(&source.dataset) {
            Err(graphrt_connect::ConnectError::Config(format!(
                "source `{}`: dataset `{}` is unknown or cannot store documents",
                source.id, source.dataset
            )))
        } else {
            registry.build(&source, &ctx)
        };
        if let Err(error) = &connector {
            eprintln!("warning: knowledge source `{}` cannot sync: {error}", source.id);
        }
        scheduler.add(source, connector);
    }
    Ok(Some(scheduler))
}
