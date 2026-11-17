//! `graphrt plugin`: list, install, remove, inspect and scaffold plugins.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::args::{PluginCmdArgs, PluginCommand, PluginLang};
use crate::error::{CliError, EXIT_OK};
use crate::extensions::{Extensions, PluginOptions, PluginSummary};

/// Execute a `graphrt plugin` subcommand.
pub fn plugin(args: PluginCmdArgs) -> Result<u8, CliError> {
    match args.command {
        PluginCommand::List { json, plugins } => list(&plugins.options(), json),
        PluginCommand::Install { source, rev, dir } => install(&source, rev.as_deref(), dir),
        PluginCommand::Remove { name, dir } => remove(&name, dir),
        PluginCommand::Info { name, json, plugins } => info(&name, &plugins.options(), json),
        PluginCommand::New { name, lang, dir } => scaffold(&name, lang, dir),
    }
}

fn io_err(e: std::io::Error) -> CliError {
    CliError::failed(e.to_string())
}

fn json_err(e: serde_json::Error) -> CliError {
    CliError::failed(e.to_string())
}

fn list(options: &PluginOptions, json: bool) -> Result<u8, CliError> {
    let extensions = Extensions::load(options)?;
    let mut out = std::io::stdout().lock();
    if json {
        let text = serde_json::to_string_pretty(&extensions.plugins).map_err(json_err)?;
        writeln!(out, "{text}").map_err(io_err)?;
        return Ok(EXIT_OK);
    }
    if extensions.plugins.is_empty() {
        writeln!(out, "no plugins found (search paths: {})", describe_paths(&options.search_paths()))
            .map_err(io_err)?;
        return Ok(EXIT_OK);
    }
    writeln!(out, "{:20}  {:10}  {:8}  NODES / TOOLS", "NAME", "VERSION", "RUNTIME").map_err(io_err)?;
    for p in &extensions.plugins {
        let detail = match &p.error {
            Some(e) => format!("ERROR: {e}"),
            None => {
                let tools = if p.tools.is_empty() { String::new() } else { format!("  tools: {}", p.tools.join(", ")) };
                format!("{}{tools}", p.nodes.join(", "))
            }
        };
        writeln!(out, "{:20}  {:10}  {:8}  {detail}", p.name, p.version, p.runtime).map_err(io_err)?;
    }
    Ok(EXIT_OK)
}

fn describe_paths(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "none".into();
    }
    paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
}

/// `~/.graphrt/plugins`, or the explicit `--dir`.
fn install_root(dir: Option<PathBuf>) -> Result<PathBuf, CliError> {
    dir.or_else(graphrt_plugin::default_install_dir)
        .ok_or_else(|| CliError::usage("cannot determine the home directory; pass `--dir <DIR>`"))
}

fn is_git_url(source: &str) -> bool {
    source.contains("://") || source.starts_with("git@") || source.ends_with(".git")
}

fn install(source: &str, rev: Option<&str>, dir: Option<PathBuf>) -> Result<u8, CliError> {
    let root = install_root(dir)?;
    let local = Path::new(source);
    let installed = if local.is_dir() {
        if rev.is_some() {
            return Err(CliError::usage("`--rev` only applies to git sources"));
        }
        graphrt_plugin::install_from_dir(local, &root).map_err(|e| CliError::failed(e.to_string()))?
    } else if is_git_url(source) {
        install_git(source, rev, &root)?
    } else {
        return Err(CliError::usage(format!("`{source}` is neither a plugin directory nor a git URL")));
    };
    // Make sure the installed copy actually loads before reporting success.
    let options = PluginOptions { dirs: vec![installed.dir.clone()], no_default: true, ..PluginOptions::default() };
    let extensions = Extensions::load(&options)?;
    let summary = extensions.plugins.iter().find(|p| p.name == installed.manifest.name);
    let mut out = std::io::stdout().lock();
    match summary {
        Some(PluginSummary { error: Some(e), .. }) => {
            return Err(CliError::failed(format!(
                "installed `{}` into {} but it does not load: {e}",
                installed.manifest.name,
                installed.dir.display()
            )));
        }
        Some(p) => writeln!(
            out,
            "installed {} {} into {} ({})",
            p.name,
            p.version,
            installed.dir.display(),
            if p.nodes.is_empty() { "no nodes".to_owned() } else { p.nodes.join(", ") }
        )
        .map_err(io_err)?,
        None => {
            writeln!(out, "installed {} into {}", installed.manifest.name, installed.dir.display()).map_err(io_err)?
        }
    }
    Ok(EXIT_OK)
}

#[cfg(feature = "plugins")]
fn install_git(source: &str, rev: Option<&str>, root: &Path) -> Result<graphrt_plugin::LoadedManifest, CliError> {
    graphrt_plugin::install_from_git(source, rev, root).map_err(|e| CliError::failed(e.to_string()))
}

#[cfg(not(feature = "plugins"))]
fn install_git(_source: &str, _rev: Option<&str>, _root: &Path) -> Result<graphrt_plugin::LoadedManifest, CliError> {
    Err(CliError::usage("installing from git requires a build with the `plugins` feature"))
}

fn remove(name: &str, dir: Option<PathBuf>) -> Result<u8, CliError> {
    if !graphrt_plugin::is_valid_name(name) {
        return Err(CliError::usage(format!("`{name}` is not a valid plugin name")));
    }
    let root = install_root(dir)?;
    let target = root.join(name);
    if graphrt_plugin::manifest_in(&target).is_none() {
        return Err(CliError::usage(format!("no installed plugin `{name}` in {}", root.display())));
    }
    std::fs::remove_dir_all(&target)
        .map_err(|e| CliError::failed(format!("cannot remove {}: {e}", target.display())))?;
    writeln!(std::io::stdout().lock(), "removed {name} from {}", root.display()).map_err(io_err)?;
    Ok(EXIT_OK)
}

fn info(name: &str, options: &PluginOptions, json: bool) -> Result<u8, CliError> {
    let extensions = Extensions::load(options)?;
    let summary = extensions
        .plugins
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| CliError::usage(format!("plugin `{name}` not found (see `graphrt plugin list`)")))?;
    let descriptors: Vec<_> =
        extensions.registry.descriptors().into_iter().filter(|d| summary.nodes.contains(&d.kind)).collect();
    let mut out = std::io::stdout().lock();
    if json {
        let value = serde_json::json!({ "plugin": summary, "nodes": descriptors });
        writeln!(out, "{}", serde_json::to_string_pretty(&value).map_err(json_err)?).map_err(io_err)?;
        return Ok(EXIT_OK);
    }
    writeln!(out, "{} {} ({})", summary.name, summary.version, summary.runtime).map_err(io_err)?;
    if !summary.description.is_empty() {
        writeln!(out, "{}", summary.description).map_err(io_err)?;
    }
    writeln!(out, "path: {}", summary.path.display()).map_err(io_err)?;
    if let Some(e) = &summary.error {
        writeln!(out, "status: failed to load: {e}").map_err(io_err)?;
    }
    for d in &descriptors {
        let outputs: Vec<&str> = d.outputs.iter().map(|o| o.name.as_str()).collect();
        writeln!(out, "  node {} - {} (outputs: {})", d.kind, d.title, outputs.join(", ")).map_err(io_err)?;
    }
    for tool in &summary.tools {
        writeln!(out, "  tool {}/{tool}", summary.name).map_err(io_err)?;
    }
    Ok(EXIT_OK)
}

const PY_SDK: &str = include_str!("../templates/python/graphrt_plugin.py");
const PY_PLUGIN: &str = include_str!("../templates/python/plugin.py");
const JS_SDK: &str = include_str!("../templates/typescript/graphrt-plugin.mjs");
const JS_PLUGIN: &str = include_str!("../templates/typescript/plugin.mjs");
const RS_MAIN: &str = include_str!("../templates/rust/main.rs");
const RS_CARGO: &str = include_str!("../templates/rust/Cargo.toml.tmpl");

fn scaffold(name: &str, lang: PluginLang, dir: Option<PathBuf>) -> Result<u8, CliError> {
    if !graphrt_plugin::is_valid_name(name) {
        return Err(CliError::usage(format!(
            "`{name}` is not a valid plugin name (1-64 lowercase letters, digits, `-` or `_`)"
        )));
    }
    let parent = dir.unwrap_or_else(|| PathBuf::from("."));
    let target = parent.join(name);
    if target.exists() {
        return Err(CliError::usage(format!("{} already exists", target.display())));
    }
    let fill = |template: &str| template.replace("{{name}}", name);
    let (command, args, files): (String, Vec<&str>, Vec<(&str, String)>) = match lang {
        PluginLang::Python => (
            "python3".into(),
            vec!["-u", "plugin.py"],
            vec![("plugin.py", fill(PY_PLUGIN)), ("graphrt_plugin.py", PY_SDK.to_owned())],
        ),
        PluginLang::Typescript => (
            "node".into(),
            vec!["plugin.mjs"],
            vec![("plugin.mjs", fill(JS_PLUGIN)), ("graphrt-plugin.mjs", JS_SDK.to_owned())],
        ),
        PluginLang::Rust => (
            format!("target/release/{name}"),
            Vec::new(),
            vec![("Cargo.toml", fill(RS_CARGO)), ("src/main.rs", fill(RS_MAIN))],
        ),
    };
    let manifest = serde_json::json!({
        "name": name,
        "version": "0.1.0",
        "description": format!("The {name} plugin"),
        "runtime": "process",
        "command": command,
        "args": args,
        "limits": { "timeout_ms": 30000 },
        "nodes": [{
            "kind": format!("{name}.greet"),
            "title": "Greet",
            "description": "Greets someone (replace with your own node)",
            "category": "transform",
            "config_schema": {
                "type": "object",
                "properties": { "name": { "type": "string", "description": "Who to greet (templates allowed)" } },
                "required": ["name"]
            },
            "outputs": [
                { "name": "text", "type": "string", "streaming": true },
                { "name": "length", "type": "integer" }
            ]
        }]
    });
    let write = |rel: &str, contents: &str| -> Result<(), CliError> {
        let path = target.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CliError::failed(format!("cannot create {}: {e}", parent.display())))?;
        }
        std::fs::write(&path, contents).map_err(|e| CliError::failed(format!("cannot write {}: {e}", path.display())))
    };
    write("graphrt-plugin.json", &serde_json::to_string_pretty(&manifest).map_err(json_err)?)?;
    for (rel, contents) in &files {
        write(rel, contents)?;
    }
    let example = format!(
        "version: \"1\"\nname: {name}-demo\ngraph:\n  nodes:\n    - {{ id: start, type: start, variables: [{{ variable: who, type: text-input, required: true }}] }}\n    - {{ id: greet, type: {name}.greet, name: \"{{{{#start.who#}}}}\" }}\n    - {{ id: answer, type: answer, answer: \"{{{{#greet.text#}}}}\" }}\n  edges:\n    - {{ source: start, target: greet }}\n    - {{ source: greet, target: answer }}\n"
    );
    write("example.yaml", &example)?;

    let mut out = std::io::stdout().lock();
    writeln!(out, "created {} ({lang:?} plugin)", target.display()).map_err(io_err)?;
    if lang == PluginLang::Rust {
        writeln!(out, "build it:  (cd {} && cargo build --release)", target.display()).map_err(io_err)?;
    }
    writeln!(
        out,
        "try it:    graphrt run {}/example.yaml --plugins {} --input who=World",
        target.display(),
        target.display()
    )
    .map_err(io_err)?;
    writeln!(out, "install:   graphrt plugin install {}", target.display()).map_err(io_err)?;
    Ok(EXIT_OK)
}
