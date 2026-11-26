//! Skill contract between a skill store and the nodes that use skills.
//!
//! A skill is a package of instructions plus optional reference files and
//! tool requests, loaded only when a task needs it. This module holds only
//! the contract: the core never reads a skill from disk. A host registers a
//! [`Skills`] service (for example the store of the `graphrt-skill` crate)
//! and the `agent` node asks it what the skills an agent lists resolve to.
//!
//! What a node gets for one run is an [`AttachedSkills`]:
//!
//! - `catalog`: one short line per skill for the system prompt;
//! - `runtime`: a tool provider scoped to those skills, with
//!   [`LOAD_SKILL_TOOL`] and [`READ_SKILL_FILE_TOOL`];
//! - `grants`: the tools the owner granted each skill.
//!
//! A skill never grants itself a tool. The grants come from the store, where
//! the owner recorded them, and a node offers a granted tool to the model only
//! after the skill was loaded in that run.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::tool::ToolRuntime;

/// Provider name of the skill tools in logs and tool references.
pub const SKILL_PROVIDER: &str = "skills";
/// Tool that returns the instructions of one skill. Argument: `{"name": "<skill>"}`.
pub const LOAD_SKILL_TOOL: &str = "load_skill";
/// Tool that returns one file of a skill. Arguments: `{"name": "<skill>", "path": "<file>"}`.
pub const READ_SKILL_FILE_TOOL: &str = "read_skill_file";

/// A tool the owner granted to a skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillToolGrant {
    /// The skill the grant belongs to.
    pub skill: String,
    /// Tool provider in the [`crate::tool::ToolRegistry`].
    pub provider: String,
    /// Tool name, or `*` for every tool of the provider.
    pub tool: String,
    /// Whether each call waits for approval first. `true` unless the owner
    /// chose otherwise.
    pub ask: bool,
}

/// What the skills an agent lists resolve to for one run.
#[derive(Clone, Debug)]
pub struct AttachedSkills {
    /// Catalog for the system prompt: how to load a skill, then one line per
    /// skill. Empty when no listed skill is enabled.
    pub catalog: String,
    /// The skill tools, scoped to the attached skills.
    pub runtime: Arc<dyn ToolRuntime>,
    /// Tools granted to the attached skills.
    pub grants: Vec<SkillToolGrant>,
}

impl AttachedSkills {
    /// Whether there is nothing to attach (every listed skill is disabled).
    pub fn is_empty(&self) -> bool {
        self.catalog.is_empty()
    }
}

/// A skill could not be attached.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SkillError {
    /// Stable error kind, for example `skill_not_found`.
    pub kind: &'static str,
    /// What went wrong, safe to show to the owner.
    pub message: String,
}

impl SkillError {
    /// An error of `kind` with `message`.
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

/// A source of installed skills.
pub trait SkillSource: Send + Sync + fmt::Debug {
    /// Resolve `names` for one run.
    ///
    /// A disabled skill is left out without an error. A name that is not
    /// installed is an error, so a typo never runs an agent without the skill
    /// its author expected.
    fn attach<'a>(&'a self, names: &'a [String]) -> BoxFuture<'a, Result<AttachedSkills, SkillError>>;
}

/// Service registered in [`crate::node::Services`]: where nodes find skills.
#[derive(Clone, Debug)]
pub struct Skills(pub Arc<dyn SkillSource>);

impl Skills {
    /// Wrap a skill source as a service.
    pub fn new(source: Arc<dyn SkillSource>) -> Self {
        Self(source)
    }
}
