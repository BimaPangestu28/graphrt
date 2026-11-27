//! Turning procedures an agent learned into a skill draft.
//!
//! Short lessons are the right place for a preference or a fact. When several
//! approved lessons describe how to do a task, they read better as one skill
//! the agent loads when that task comes up. [`draft_skill_from_lessons`] asks
//! a model to write that skill from the lessons.
//!
//! Only lessons that are **approved** and apply to **every run of the agent**
//! are used. A proposal is not trusted yet, and a lesson of one user may hold
//! that user's data, which must not end up in a skill every run can load.
//!
//! The result is a [`SkillDraft`] like any other: a person reads it and it is
//! installed through [`graphrt_skill::SkillStore::install_draft`], with the
//! same scan and no tool granted.

use graphrt_core::text_model::TextModel;
use graphrt_skill::{DraftRequest, DraftSource, SkillDraft, draft_skill};

use crate::error::Error;
use crate::lesson::{Lesson, LessonKind, LessonStatus};
use crate::store::{LessonFilter, LessonStore, UserFilter};

/// Fewest procedure lessons worth turning into a skill.
pub const MIN_PROCEDURES: usize = 3;
/// Most lessons handed to the model.
pub const MAX_PROCEDURES: usize = 40;

/// The lessons of `agent` a skill may be written from: approved, for every
/// run, and about how to do something (kinds procedure and correction).
///
/// # Errors
/// A store error.
pub fn procedure_lessons(store: &LessonStore, agent: &str) -> Result<Vec<Lesson>, Error> {
    let filter = LessonFilter { user: UserFilter::AgentWide, status: Some(LessonStatus::Approved) };
    let mut lessons = store.list(agent, &filter)?;
    lessons.retain(|lesson| matches!(lesson.kind, LessonKind::Procedure | LessonKind::Correction));
    lessons.truncate(MAX_PROCEDURES);
    Ok(lessons)
}

/// Ask `model` to write a skill from the procedures `agent` learned.
///
/// Returns `None` when there are fewer than [`MIN_PROCEDURES`] such lessons.
/// `available_tools` are the tools the draft may ask for.
///
/// # Errors
/// A store error, or [`Error::SkillDraft`] when the model fails or its answer
/// is not a skill.
pub async fn draft_skill_from_lessons(
    store: &LessonStore,
    agent: &str,
    model: &dyn TextModel,
    available_tools: &[String],
) -> Result<Option<SkillDraft>, Error> {
    let lessons = procedure_lessons(store, agent)?;
    if lessons.len() < MIN_PROCEDURES {
        return Ok(None);
    }
    let mut description = String::from(
        "These are things an agent learned about how to do its work, each approved by its owner. Write one \
         skill that covers them as a procedure.\n",
    );
    for lesson in &lessons {
        description.push_str("\n- ");
        description.push_str(&lesson.text);
    }
    let request =
        DraftRequest { source: DraftSource::Description(description), available_tools: available_tools.to_vec() };
    let draft = draft_skill(model, &request).await.map_err(|error| Error::SkillDraft(error.to_string()))?;
    Ok(Some(draft))
}
