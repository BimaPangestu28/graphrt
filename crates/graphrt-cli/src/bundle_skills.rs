//! Skills inside a bundle (`.grt`), for builds with the `skills` and `bundle`
//! features.
//!
//! ```text
//! skills/index.json        which skills, their grants, whether findings were accepted
//! skills/<name>/SKILL.md   the skill's files, exactly as installed
//! skills/<name>/...
//! ```
//!
//! **Building.** `graphrt bundle build` packs every skill the workflow's
//! `agent` nodes list, taken from the skill store with its pinned content and
//! the grants the owner gave there.
//!
//! **Running.** The bundled skills are installed into a new private skill
//! store, through the normal install (validation, scan, grant rules), and that
//! store replaces the default one for the run. A bundled agent sees the
//! bundled skills and nothing else.
//!
//! The grants travel with the bundle because they belong to its author, like
//! the `tools` an agent lists in the workflow of the same bundle. The bundle
//! manifest pins every file, the index included, so a signed bundle covers
//! them. A grant for a tool the skill does not ask for is still refused.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use graphrt_skill::{Grant, InstallOptions, OriginKind, SkillOrigin, SkillStore};
use serde::{Deserialize, Serialize};

use crate::error::CliError;

/// Path of the skill index inside a bundle.
pub const SKILL_INDEX_PATH: &str = "skills/index.json";
const SKILLS_PREFIX: &str = "skills";
const INDEX_SCHEMA: u32 = 1;

/// `skills/index.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledSkills {
    schema: u32,
    skills: BTreeMap<String, BundledSkill>,
}

/// One bundled skill.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledSkill {
    #[serde(default)]
    grants: Vec<Grant>,
    /// The owner installed it although the scan found something blocking.
    #[serde(default)]
    accept_findings: bool,
}

/// The skills the `agent` nodes of `workflow` list, without duplicates.
pub fn listed_skills(workflow: &graphrt_core::Workflow) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for node in workflow.graph.nodes.iter().filter(|node| node.kind == "agent") {
        let Some(listed) = node.config.get("skills").and_then(serde_json::Value::as_array) else { continue };
        for name in listed.iter().filter_map(serde_json::Value::as_str) {
            if !names.iter().any(|known| known == name) {
                names.push(name.to_owned());
            }
        }
    }
    names
}

/// The files to add to a bundle for `names`, read from `store`: every file of
/// each skill under `skills/<name>/`, then the index.
///
/// # Errors
/// A usage error when a listed skill is not installed, a failure when its
/// files changed since they were installed.
pub fn files_for_bundle(store: &SkillStore, names: &[String]) -> Result<Vec<(String, Vec<u8>)>, CliError> {
    let mut files = Vec::new();
    let mut index = BundledSkills { schema: INDEX_SCHEMA, skills: BTreeMap::new() };
    for name in names {
        let entry = store.get(name).map_err(|error| {
            CliError::usage(format!("{error}: install it with `graphrt skill install` before building the bundle"))
        })?;
        for (path, content) in store.export(name).map_err(|error| CliError::failed(error.to_string()))? {
            files.push((format!("{SKILLS_PREFIX}/{name}/{path}"), content));
        }
        let accept_findings = entry.findings.iter().any(graphrt_skill::Finding::is_blocking);
        index.skills.insert(name.clone(), BundledSkill { grants: entry.grants, accept_findings });
    }
    let text = serde_json::to_vec_pretty(&index).map_err(|error| CliError::failed(error.to_string()))?;
    files.push((SKILL_INDEX_PATH.to_owned(), text));
    Ok(files)
}

/// Install the skills of `bundle` into a new store under `private_dir` and
/// return that store's directory, or `None` when the bundle has no skills.
///
/// # Errors
/// An invalid bundle error when the index cannot be read, a skill it names is
/// missing, or a skill is refused by the install (bad content, a blocking
/// finding that was not accepted, a grant for a tool it does not ask for).
pub fn install_from_bundle(bundle: &graphrt_bundle::Bundle, private_dir: &Path) -> Result<Option<PathBuf>, CliError> {
    let Some(index) = bundle.file(SKILL_INDEX_PATH) else { return Ok(None) };
    let invalid = |why: String| CliError::invalid(format!("bundle skills: {why}"));
    let index: BundledSkills = serde_json::from_slice(index).map_err(|error| invalid(error.to_string()))?;
    if index.schema != INDEX_SCHEMA {
        return Err(invalid(format!("index format {} is not supported", index.schema)));
    }
    let source = private_dir.join("skill-source");
    let store_dir = private_dir.join("skill-store");
    let store = SkillStore::open(&store_dir);
    let signed = bundle.signer().is_some();
    for (name, bundled) in index.skills {
        if !graphrt_skill::is_valid_name(&name) {
            return Err(invalid("a skill name is not valid".to_owned()));
        }
        let prefix = format!("{SKILLS_PREFIX}/{name}/");
        let dir = source.join(&name);
        let mut found = false;
        for path in bundle.paths().filter(|path| path.starts_with(&prefix)).map(str::to_owned).collect::<Vec<_>>() {
            let relative = path.strip_prefix(&prefix).unwrap_or_default();
            // The install checks every name again. This only keeps the copy inside `dir`.
            if relative.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
                return Err(invalid(format!("`{path}` is not a safe path")));
            }
            let target = relative.split('/').fold(dir.clone(), |target, part| target.join(part));
            let io =
                |error: std::io::Error| CliError::failed(format!("cannot extract bundled skill `{name}`: {error}"));
            if let Some(parent) = target.parent() {
                graphrt_core::private_fs::create_dir_all(parent).map_err(io)?;
            }
            graphrt_core::private_fs::write(&target, bundle.file(&path).unwrap_or_default()).map_err(io)?;
            found = true;
        }
        if !found {
            return Err(invalid(format!("skill `{name}` is in the index but has no files")));
        }
        let origin = SkillOrigin {
            kind: OriginKind::Local,
            detail: Some(format!("bundle {}", bundle.manifest().name)),
            publisher: None,
            verified: signed,
        };
        let options =
            InstallOptions { grants: bundled.grants, accept_findings: bundled.accept_findings, replace: false, origin };
        let entry = store.install(&dir, options).map_err(|error| invalid(error.to_string()))?;
        if entry.name != name {
            return Err(invalid(format!("the skill under `{prefix}` is named `{}`", entry.name)));
        }
    }
    Ok(Some(store_dir))
}
