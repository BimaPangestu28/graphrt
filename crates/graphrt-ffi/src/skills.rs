//! Agent skills (feature `skills`): the `skills` field of `agent` nodes, served
//! from a store in the app's own storage (config `skills_dir`). An app installs
//! a skill from its `SKILL.md` text; no code of a skill ever runs.
//!
//! The methods exist in every build, so the bindings are the same for every
//! feature set; without the feature, or without `skills_dir`, they fail with
//! `GraphrtError.Unsupported`.

use crate::{GraphrtError, Runtime};

/// A tool granted to a skill.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SkillGrant {
    /// `provider/tool`, or `*` as the tool for every tool of the provider, exactly as the skill asks for it.
    pub tool: String,
    /// Calls run without asking; otherwise every call waits for approval.
    pub allow: bool,
}

/// What to install.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SkillInstall {
    /// The whole `SKILL.md` (front matter and instructions).
    pub skill_md: String,
    /// Tools to grant; each must be one the skill asks for in `allowed-tools`.
    #[uniffi(default)]
    pub grants: Vec<SkillGrant>,
    /// Replace an installed skill of the same name.
    #[uniffi(default)]
    pub replace: bool,
    /// Install although the scan found something blocking.
    #[uniffi(default)]
    pub accept_findings: bool,
}

/// An installed skill.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub title: Option<String>,
    pub enabled: bool,
    /// Tools the skill asks for.
    pub requested_tools: Vec<String>,
    /// Tools granted to it.
    pub grants: Vec<SkillGrant>,
    /// What the scan found, one line each (`severity code: message`).
    pub findings: Vec<String>,
    pub version: Option<String>,
}

#[cfg(feature = "skills")]
mod imp {
    use std::sync::Arc;

    use graphrt_skill::{Grant, GrantMode, InstallOptions, OriginKind, SkillEntry, SkillOrigin, SkillStore};

    use super::{SkillGrant, SkillInfo, SkillInstall};
    use crate::GraphrtError;

    pub(crate) type Store = Arc<SkillStore>;

    fn failed(e: graphrt_skill::Error) -> GraphrtError {
        GraphrtError::input(e.to_string())
    }

    /// Open the store and register the service `agent` nodes read skills from.
    pub(crate) fn install(dir: &str, services: &mut graphrt_core::Services) -> Store {
        let store = Arc::new(SkillStore::open(dir));
        services.insert(graphrt_skill::SkillSet::new(Arc::clone(&store)).into_service());
        store
    }

    fn info(entry: SkillEntry) -> SkillInfo {
        SkillInfo {
            name: entry.name,
            description: entry.description,
            title: entry.title,
            enabled: entry.enabled,
            requested_tools: entry.requested_tools,
            grants: entry
                .grants
                .into_iter()
                .map(|g| SkillGrant { tool: g.tool, allow: g.mode == GrantMode::Allow })
                .collect(),
            findings: entry
                .findings
                .iter()
                .map(|f| format!("{:?} {}: {}", f.severity, f.code, f.message).to_lowercase())
                .collect(),
            version: entry.version,
        }
    }

    pub(crate) fn install_skill(store: &SkillStore, request: SkillInstall) -> Result<SkillInfo, GraphrtError> {
        let package = graphrt_skill::package::package_from_skill_file(&request.skill_md).map_err(failed)?;
        let grants = request
            .grants
            .into_iter()
            .map(|g| if g.allow { Grant::allow(g.tool) } else { Grant::ask(g.tool) })
            .collect();
        let options = InstallOptions {
            grants,
            accept_findings: request.accept_findings,
            replace: request.replace,
            origin: SkillOrigin {
                kind: OriginKind::Authored,
                detail: Some("app".to_owned()),
                ..SkillOrigin::default()
            },
        };
        store.install_package(&package, options).map(info).map_err(failed)
    }

    pub(crate) fn list(store: &SkillStore) -> Result<Vec<SkillInfo>, GraphrtError> {
        Ok(store.list().map_err(failed)?.into_iter().map(info).collect())
    }

    pub(crate) fn remove(store: &SkillStore, name: &str) -> Result<(), GraphrtError> {
        store.remove(name).map_err(failed)
    }

    pub(crate) fn set_enabled(store: &SkillStore, name: &str, enabled: bool) -> Result<SkillInfo, GraphrtError> {
        store.set_enabled(name, enabled).map(info).map_err(failed)
    }
}

#[cfg(feature = "skills")]
pub(crate) use imp::{Store, install};

#[cfg(not(feature = "skills"))]
pub(crate) type Store = ();

impl Runtime {
    #[cfg(feature = "skills")]
    fn skill_store(&self) -> Result<&Store, GraphrtError> {
        self.skills.as_ref().ok_or_else(|| GraphrtError::not_configured("skills", "skills_dir"))
    }
}

#[uniffi::export]
impl Runtime {
    /// Install a skill from its `SKILL.md`. It is enabled once installed.
    pub fn install_skill(&self, request: SkillInstall) -> Result<SkillInfo, GraphrtError> {
        #[cfg(feature = "skills")]
        {
            imp::install_skill(self.skill_store()?, request)
        }
        #[cfg(not(feature = "skills"))]
        {
            let _ = request;
            Err(GraphrtError::unsupported("skills", "skills"))
        }
    }

    /// Every installed skill.
    pub fn list_skills(&self) -> Result<Vec<SkillInfo>, GraphrtError> {
        #[cfg(feature = "skills")]
        {
            imp::list(self.skill_store()?)
        }
        #[cfg(not(feature = "skills"))]
        {
            Err(GraphrtError::unsupported("skills", "skills"))
        }
    }

    /// Turn a skill on or off for every agent.
    pub fn set_skill_enabled(&self, name: String, enabled: bool) -> Result<SkillInfo, GraphrtError> {
        #[cfg(feature = "skills")]
        {
            imp::set_enabled(self.skill_store()?, &name, enabled)
        }
        #[cfg(not(feature = "skills"))]
        {
            let _ = (name, enabled);
            Err(GraphrtError::unsupported("skills", "skills"))
        }
    }

    /// Remove an installed skill.
    pub fn remove_skill(&self, name: String) -> Result<(), GraphrtError> {
        #[cfg(feature = "skills")]
        {
            imp::remove(self.skill_store()?, &name)
        }
        #[cfg(not(feature = "skills"))]
        {
            let _ = name;
            Err(GraphrtError::unsupported("skills", "skills"))
        }
    }
}
