//! # graphrt-skill
//!
//! Agent Skills for graphrt. A skill is a directory with a `SKILL.md` file
//! (the [Agent Skills](https://agentskills.io/specification) format): a name,
//! a description, instructions, and optional reference files. An agent that
//! lists a skill sees one line about it and loads the instructions only when
//! a task needs them.
//!
//! - [`manifest`]: parse and validate `SKILL.md`.
//! - [`package`]: read a skill directory once, with limits, and find the
//!   skills of an Agent Plugins package.
//! - [`scan`]: plain language findings shown before an install.
//! - [`store`]: install, list, grant, remove. Every file is pinned by hash.
//! - [`runtime`]: the catalog and the `load_skill` / `read_skill_file` tools.
//!
//! What a skill can do is limited by construction, not by the scan:
//!
//! - **No code runs.** Files under `scripts/` are text like any other file.
//! - **A skill grants itself nothing.** It asks for tools in `allowed-tools`.
//!   The owner grants a subset, stored in the store's index.
//! - **Content is pinned.** A file that changed after the install is refused.
//!
//! ```no_run
//! use std::sync::Arc;
//! use graphrt_skill::{Grant, InstallOptions, SkillSet, SkillStore};
//!
//! # fn main() -> Result<(), graphrt_skill::Error> {
//! let store = Arc::new(SkillStore::open("/var/lib/graphrt/skills"));
//! let review = store.inspect_dir("downloads/refund-policy".as_ref())?;
//! println!("{} asks for {:?}", review.entry.name, review.entry.requested_tools);
//! let options = InstallOptions { grants: vec![Grant::ask("orders/lookup")], ..InstallOptions::default() };
//! store.install("downloads/refund-policy".as_ref(), options)?;
//! // Register with the engine's services so `agent` nodes can list `skills: [refund-policy]`.
//! let service = SkillSet::new(store).into_service();
//! # let _ = service;
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod manifest;
pub mod package;
pub mod runtime;
pub mod scan;
pub mod store;
pub mod text;

pub use error::Error;
pub use manifest::{ParsedSkill, SKILL_FILE, SkillManifest, is_valid_name, parse_skill_file};
pub use package::{Discovered, PackageLimits, PluginPackage, SkillFile, SkillPackage, discover, read_package};
pub use runtime::SkillSet;
pub use scan::{Finding, Severity};
pub use store::{
    Grant, GrantMode, InstallOptions, LoadedSkill, OriginKind, SkillChanges, SkillDetails, SkillEntry, SkillOrigin,
    SkillStore,
};
