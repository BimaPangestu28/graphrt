//! `graphrt skill`: list, inspect, install, grant, enable, disable and remove
//! agent skills (cargo feature `skills`).
//!
//! Every subcommand works on one skill store: `--dir`, else
//! `$GRAPHRT_SKILLS_DIR`, else `~/.graphrt/skills`. `graphrt run` and
//! `graphrt serve` read the same default store for `agent` nodes that list
//! `skills`.

use std::io::Write;
use std::path::{Path, PathBuf};

use graphrt_skill::{
    Discovered, DraftRequest, DraftSource, Error as SkillError, Finding, Grant, InstallOptions, OriginKind, Severity,
    SkillDetails, SkillEntry, SkillOrigin, SkillStore, Turn, discover, draft_skill,
};

use crate::args::{SkillCmdArgs, SkillCommand, SkillGrantArgs};
use crate::error::{CliError, EXIT_OK};

/// Execute a `graphrt skill` subcommand.
pub async fn skill(args: SkillCmdArgs) -> Result<u8, CliError> {
    // Drafting calls a model first: do it before taking the output lock.
    let command = match args.command {
        SkillCommand::Draft { model, describe, conversation, tools, out, providers } => {
            let request = DraftArgs { model, describe, conversation, tools, out, providers };
            return draft(request, args.json).await;
        }
        other => other,
    };
    let store = open_store(args.dir)?;
    let mut out = std::io::stdout().lock();
    match command {
        SkillCommand::Draft { .. } => Ok(EXIT_OK),
        SkillCommand::List => list(&store, args.json, &mut out),
        SkillCommand::Inspect { skill } => inspect(&store, &skill, args.json, &mut out),
        SkillCommand::Install { source, index, index_key, grants, accept_findings, replace } => {
            let grants = grants_of(&grants);
            let path = Path::new(&source);
            if path.is_dir() {
                if index.is_some() || index_key.is_some() {
                    return Err(CliError::usage("`--index` and `--index-key` only apply to a catalog skill name"));
                }
                let request = InstallRequest { path, grants, accept_findings, replace, catalog: None };
                return install(&store, &request, args.json, &mut out);
            }
            let catalog = CatalogSource { index: index.as_deref(), index_key: index_key.as_deref() };
            install_from_catalog(&store, &source, catalog, grants, (accept_findings, replace), args.json, &mut out)
        }
        SkillCommand::Search { query, index, index_key } => {
            let catalog = CatalogSource { index: index.as_deref(), index_key: index_key.as_deref() };
            search(query.as_deref(), catalog, &mut out)
        }
        SkillCommand::Grant { name, grants } => {
            let entry = store.set_grants(&name, grants_of(&grants)).map_err(failed)?;
            print_entry(&entry, args.json, &mut out)
        }
        SkillCommand::Enable { name } => {
            let entry = store.set_enabled(&name, true).map_err(failed)?;
            print_entry(&entry, args.json, &mut out)
        }
        SkillCommand::Disable { name } => {
            let entry = store.set_enabled(&name, false).map_err(failed)?;
            print_entry(&entry, args.json, &mut out)
        }
        SkillCommand::Remove { name } => {
            store.remove(&name).map_err(failed)?;
            if !args.json {
                writeln!(out, "removed skill `{name}`").map_err(io_error)?;
            }
            Ok(EXIT_OK)
        }
    }
}

struct DraftArgs {
    model: String,
    describe: Option<String>,
    conversation: Option<PathBuf>,
    tools: Vec<String>,
    out: PathBuf,
    providers: Option<PathBuf>,
}

/// `graphrt skill draft`: ask a model for a skill and write it as a folder.
async fn draft(args: DraftArgs, json: bool) -> Result<u8, CliError> {
    let source = match (&args.describe, &args.conversation) {
        (Some(text), _) => DraftSource::Description(text.clone()),
        (None, Some(path)) => DraftSource::Conversation(read_conversation(path)?),
        (None, None) => return Err(CliError::usage("pass `--describe <TEXT>` or `--conversation <FILE>`")),
    };
    let (provider, name) = crate::setup::parse_model_override(&args.model)?;
    let options = crate::setup::ServiceOptions { providers: args.providers.clone(), ..Default::default() };
    let services = crate::setup::services(&options, graphrt_core::tool::ToolRegistry::new()).await?;
    let registry = services
        .get::<graphrt_llm::ModelRegistry>()
        .ok_or_else(|| CliError::failed("no model providers are configured"))?;
    let chat = registry.get(&provider).map_err(|error| CliError::usage(error.to_string()))?;
    let model = graphrt_llm::text_model::ChatTextModel::new(chat, name);
    let request = DraftRequest { source, available_tools: args.tools };
    let draft = draft_skill(&model, &request).await.map_err(failed)?;
    let dir = draft.write_to(&args.out).map_err(failed)?;
    let mut out = std::io::stdout().lock();
    if json {
        let value = serde_json::json!({ "dir": dir, "draft": draft });
        return print_json(&value, &mut out);
    }
    writeln!(out, "wrote a draft of skill `{}` to {}", draft.name, dir.display()).map_err(io_error)?;
    writeln!(out, "  {}", draft.description).map_err(io_error)?;
    if !draft.requested_tools.is_empty() {
        writeln!(out, "  asks for tools: {} (none is granted by installing)", draft.requested_tools.join(", "))
            .map_err(io_error)?;
    }
    print_findings(&draft.findings, &mut out)?;
    writeln!(
        out,
        "read and edit {}, then run `graphrt skill install {}`",
        dir.join("SKILL.md").display(),
        dir.display()
    )
    .map_err(io_error)?;
    Ok(EXIT_OK)
}

/// An example conversation from a JSON file: an array of objects with `role`
/// and `content` (or `text`). Other shapes are usage errors.
fn read_conversation(path: &Path) -> Result<Vec<Turn>, CliError> {
    const MAX_BYTES: u64 = 4 * 1024 * 1024;
    let invalid = |why: &str| CliError::usage(format!("conversation file `{}`: {why}", path.display()));
    let size = std::fs::metadata(path).map_err(|error| invalid(&error.to_string()))?.len();
    if size > MAX_BYTES {
        return Err(invalid("larger than 4 MiB"));
    }
    let text = std::fs::read_to_string(path).map_err(|error| invalid(&error.to_string()))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| invalid(&error.to_string()))?;
    let items = value.as_array().ok_or_else(|| invalid("expected a JSON array of {role, content} objects"))?;
    let mut turns = Vec::with_capacity(items.len());
    for item in items {
        let role = item.get("role").and_then(serde_json::Value::as_str).unwrap_or("user");
        let content = item.get("content").or_else(|| item.get("text")).and_then(serde_json::Value::as_str);
        let content = content.ok_or_else(|| invalid("every entry needs a `content` string"))?;
        turns.push(Turn { role: role.to_owned(), text: content.to_owned() });
    }
    Ok(turns)
}

struct InstallRequest<'a> {
    path: &'a Path,
    grants: Vec<Grant>,
    accept_findings: bool,
    replace: bool,
    /// Set for a skill fetched from the signed catalog: its origin.
    catalog: Option<SkillOrigin>,
}

/// `--index` and `--index-key` of the catalog subcommands.
#[derive(Clone, Copy)]
struct CatalogSource<'a> {
    index: Option<&'a str>,
    index_key: Option<&'a str>,
}

/// `graphrt skill install <name>[@version]`: fetch the skill from the signed
/// catalog, then install the unpacked directory like a local one.
#[cfg(feature = "plugins")]
fn install_from_catalog(
    store: &SkillStore,
    source: &str,
    catalog: CatalogSource<'_>,
    grants: Vec<Grant>,
    (accept_findings, replace): (bool, bool),
    json: bool,
    out: &mut impl Write,
) -> Result<u8, CliError> {
    use crate::plugin_catalog::skills::{SkillCatalogSource, checked_origin, fetch_catalog_skill};
    let spec = crate::plugin_catalog::index::parse_plugin_spec(source)
        .filter(|spec| graphrt_skill::is_valid_name(&spec.name))
        .ok_or_else(|| {
            CliError::usage(format!(
                "`{source}` is neither a skill directory nor a catalog skill name (`name` or `name@1.2.3`)"
            ))
        })?;
    let source = SkillCatalogSource { index: catalog.index, index_key: catalog.index_key, state_file: None };
    let fetched = fetch_catalog_skill(&spec, source)?;
    // The archive must hold the skill the signed index describes.
    let origin = checked_origin(store, &fetched)?;
    let request = InstallRequest { path: &fetched.dir, grants, accept_findings, replace, catalog: Some(origin) };
    install(store, &request, json, out)
}

#[cfg(not(feature = "plugins"))]
fn install_from_catalog(
    _store: &SkillStore,
    source: &str,
    _catalog: CatalogSource<'_>,
    _grants: Vec<Grant>,
    _flags: (bool, bool),
    _json: bool,
    _out: &mut impl Write,
) -> Result<u8, CliError> {
    Err(CliError::usage(format!(
        "`{source}` is not a directory. Installing a skill from the catalog by name needs a build with the `plugins` feature"
    )))
}

/// `graphrt skill search [query]`.
fn search(query: Option<&str>, catalog: CatalogSource<'_>, out: &mut impl Write) -> Result<u8, CliError> {
    #[cfg(feature = "plugins")]
    {
        use crate::plugin_catalog::skills::{SkillCatalogSource, search_catalog_skills};
        let source = SkillCatalogSource { index: catalog.index, index_key: catalog.index_key, state_file: None };
        search_catalog_skills(query, source, out)?;
        Ok(EXIT_OK)
    }
    #[cfg(not(feature = "plugins"))]
    {
        let _ = (query, catalog.index, catalog.index_key, out);
        Err(CliError::usage("searching the skill catalog needs a build with the `plugins` feature"))
    }
}

fn open_store(dir: Option<PathBuf>) -> Result<SkillStore, CliError> {
    let root = dir.or_else(SkillStore::default_root).ok_or_else(|| {
        CliError::usage(
            "cannot find a home directory for the skill store; pass `--dir <DIR>` or set GRAPHRT_SKILLS_DIR",
        )
    })?;
    Ok(SkillStore::open(root))
}

fn grants_of(args: &SkillGrantArgs) -> Vec<Grant> {
    let asking = args.ask.iter().filter(|tool| !args.allow.contains(tool)).map(Grant::ask);
    asking.chain(args.allow.iter().map(Grant::allow)).collect()
}

fn io_error(error: std::io::Error) -> CliError {
    CliError::failed(error.to_string())
}

fn failed(error: SkillError) -> CliError {
    match error {
        SkillError::NotFound(_) | SkillError::NotASkill(_) | SkillError::GrantNotRequested { .. } => {
            CliError::usage(error.to_string())
        }
        other => CliError::failed(other.to_string()),
    }
}

fn print_json(value: &impl serde::Serialize, out: &mut impl Write) -> Result<u8, CliError> {
    let text = serde_json::to_string_pretty(value).map_err(|e| CliError::failed(e.to_string()))?;
    writeln!(out, "{text}").map_err(io_error)?;
    Ok(EXIT_OK)
}

fn list(store: &SkillStore, json: bool, out: &mut impl Write) -> Result<u8, CliError> {
    let skills = store.list().map_err(failed)?;
    if json {
        return print_json(&skills, out);
    }
    if skills.is_empty() {
        writeln!(out, "no skills installed in {}", store.root().display()).map_err(io_error)?;
        return Ok(EXIT_OK);
    }
    writeln!(out, "{:28}  {:8}  {:6}  DESCRIPTION", "NAME", "STATE", "TOOLS").map_err(io_error)?;
    for entry in &skills {
        let state = if entry.enabled { "on" } else { "off" };
        let tools = format!("{}/{}", entry.grants.len(), entry.requested_tools.len());
        let description = graphrt_skill::text::shorten(&entry.description, 70);
        writeln!(out, "{:28}  {state:8}  {tools:6}  {description}", entry.name).map_err(io_error)?;
    }
    Ok(EXIT_OK)
}

fn inspect(store: &SkillStore, skill: &str, json: bool, out: &mut impl Write) -> Result<u8, CliError> {
    let path = Path::new(skill);
    let details = if path.is_dir() {
        let found = discover(path).map_err(failed)?;
        let details: Result<Vec<SkillDetails>, SkillError> =
            found.skills.iter().map(|dir| store.inspect_dir(dir)).collect();
        let details = details.map_err(failed)?;
        if !json {
            print_package_notes(&found, out)?;
        }
        details
    } else {
        vec![store.inspect(skill).map_err(failed)?]
    };
    if json {
        return print_json(&details, out);
    }
    for detail in &details {
        print_details(detail, out)?;
    }
    Ok(EXIT_OK)
}

fn install(store: &SkillStore, request: &InstallRequest<'_>, json: bool, out: &mut impl Write) -> Result<u8, CliError> {
    let found = discover(request.path).map_err(failed)?;
    // Read every skill first: nothing is installed when one of them is refused.
    let reviews: Result<Vec<SkillDetails>, SkillError> =
        found.skills.iter().map(|dir| store.inspect_dir(dir)).collect();
    let reviews = reviews.map_err(failed)?;
    if let Some(unclaimed) =
        request.grants.iter().find(|grant| !reviews.iter().any(|r| r.entry.requested_tools.contains(&grant.tool)))
    {
        return Err(CliError::usage(format!(
            "no skill in {} asks for tool `{}`, so it cannot be granted",
            request.path.display(),
            unclaimed.tool
        )));
    }
    if let Some(blocked) = reviews.iter().find(|review| review.blocked && !request.accept_findings) {
        if !json {
            print_details(blocked, out)?;
        }
        return Err(CliError::failed(format!(
            "skill `{}` was not installed: read the findings above, then pass `--accept-findings` to install it anyway",
            blocked.entry.name
        )));
    }
    if let Some(existing) = reviews.iter().find(|review| review.changes.is_some() && !request.replace) {
        return Err(CliError::usage(format!(
            "skill `{}` is already installed; pass `--replace` to update it (its grants are not kept)",
            existing.entry.name
        )));
    }
    let origin = request.catalog.clone().unwrap_or_else(|| SkillOrigin {
        kind: OriginKind::Local,
        detail: Some(request.path.display().to_string()),
        publisher: found.plugin.as_ref().and_then(|plugin| plugin.name.clone()),
        verified: false,
        version: None,
    });
    let mut installed = Vec::with_capacity(found.skills.len());
    for (dir, review) in found.skills.iter().zip(&reviews) {
        let grants =
            request.grants.iter().filter(|g| review.entry.requested_tools.contains(&g.tool)).cloned().collect();
        let options = InstallOptions {
            grants,
            accept_findings: request.accept_findings,
            replace: request.replace,
            origin: origin.clone(),
        };
        installed.push(store.install(dir, options).map_err(failed)?);
    }
    if json {
        return print_json(&installed, out);
    }
    print_package_notes(&found, out)?;
    for entry in &installed {
        writeln!(out, "installed skill `{}` into {}", entry.name, store.root().display()).map_err(io_error)?;
        print_tools(entry, out)?;
        print_findings(&entry.findings, out)?;
    }
    Ok(EXIT_OK)
}

fn print_entry(entry: &SkillEntry, json: bool, out: &mut impl Write) -> Result<u8, CliError> {
    if json {
        return print_json(entry, out);
    }
    let state = if entry.enabled { "on" } else { "off" };
    writeln!(out, "skill `{}` is {state}", entry.name).map_err(io_error)?;
    print_tools(entry, out)?;
    Ok(EXIT_OK)
}

/// What an Agent Plugins package holds besides skills, which graphrt leaves alone.
fn print_package_notes(found: &Discovered, out: &mut impl Write) -> Result<(), CliError> {
    let Some(plugin) = &found.plugin else { return Ok(()) };
    let name = plugin.name.as_deref().unwrap_or("unnamed");
    writeln!(out, "Agent Plugins package `{name}` with {} skill(s)", found.skills.len()).map_err(io_error)?;
    if plugin.has_mcp_servers {
        writeln!(out, "  note: its MCP servers (mcp.json) are not installed from here; add them with `--mcp`")
            .map_err(io_error)?;
    }
    if plugin.has_hooks {
        writeln!(out, "  note: its hooks (hooks.json) are not run by graphrt").map_err(io_error)?;
    }
    Ok(())
}

fn print_details(details: &SkillDetails, out: &mut impl Write) -> Result<(), CliError> {
    let entry = &details.entry;
    // `changes` is only set when a directory is reviewed against an installed skill.
    let state = match (&details.changes, details.installed, entry.enabled) {
        (Some(changes), _, _) if changes.is_empty() => "this version is installed",
        (Some(_), _, _) => "not installed (would replace the installed version)",
        (None, true, true) => "installed, on",
        (None, true, false) => "installed, off",
        (None, false, _) => "not installed",
    };
    if let Some(title) = &entry.title {
        writeln!(out, "{title}").map_err(io_error)?;
    }
    writeln!(out, "{}  [{state}]", entry.name).map_err(io_error)?;
    writeln!(out, "  {}", entry.description).map_err(io_error)?;
    for (label, value) in [
        ("version", &entry.version),
        ("author", &entry.author),
        ("license", &entry.license),
        ("needs", &entry.compatibility),
    ] {
        if let Some(value) = value {
            writeln!(out, "  {label}: {value}").map_err(io_error)?;
        }
    }
    print_tools(entry, out)?;
    writeln!(out, "  files ({}):", entry.files.len()).map_err(io_error)?;
    for file in &entry.files {
        writeln!(out, "    {}  ({} bytes)", file.path, file.bytes).map_err(io_error)?;
    }
    print_findings(&entry.findings, out)?;
    if let Some(changes) = details.changes.as_ref().filter(|changes| !changes.is_empty()) {
        writeln!(out, "  compared with the installed version:").map_err(io_error)?;
        for (label, items) in [
            ("asks for new tools", &changes.added_tools),
            ("no longer asks for", &changes.removed_tools),
            ("changed files", &changes.changed_files),
            ("new files", &changes.added_files),
            ("removed files", &changes.removed_files),
        ] {
            if !items.is_empty() {
                writeln!(out, "    {label}: {}", items.join(", ")).map_err(io_error)?;
            }
        }
    }
    Ok(())
}

fn print_tools(entry: &SkillEntry, out: &mut impl Write) -> Result<(), CliError> {
    if entry.requested_tools.is_empty() && entry.unsupported_tools.is_empty() {
        return writeln!(out, "  tools: asks for none").map_err(io_error);
    }
    writeln!(out, "  tools it asks for:").map_err(io_error)?;
    for tool in &entry.requested_tools {
        let status = match entry.grants.iter().find(|grant| &grant.tool == tool) {
            Some(grant) if grant.mode == graphrt_skill::GrantMode::Allow => "granted, runs without asking",
            Some(_) => "granted, asks before each call",
            None => "not granted",
        };
        writeln!(out, "    {tool}  ({status})").map_err(io_error)?;
    }
    for tool in &entry.unsupported_tools {
        writeln!(out, "    {tool}  (another product's tool, cannot be granted)").map_err(io_error)?;
    }
    Ok(())
}

fn print_findings(findings: &[Finding], out: &mut impl Write) -> Result<(), CliError> {
    if findings.is_empty() {
        return Ok(());
    }
    writeln!(out, "  findings:").map_err(io_error)?;
    for finding in findings {
        let level = match finding.severity {
            Severity::Blocking => "STOP",
            Severity::Warning => "warn",
            Severity::Info => "note",
        };
        let place = match (&finding.file, finding.line) {
            (Some(file), Some(line)) => format!(" ({file}:{line})"),
            (Some(file), None) => format!(" ({file})"),
            _ => String::new(),
        };
        writeln!(out, "    [{level}] {}{place}", finding.message).map_err(io_error)?;
    }
    Ok(())
}
