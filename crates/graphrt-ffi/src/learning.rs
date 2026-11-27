//! Self learning (feature `learning`): the `learning` field of `agent` nodes
//! reads approved lessons from a store in the app's own storage (config
//! `learning_dir`). The app proposes lessons from what a person said about a
//! run, and the owner approves or rejects them; only approved lessons reach a
//! model.
//!
//! The methods exist in every build; without the feature, or without
//! `learning_dir`, they fail with `GraphrtError.Unsupported`.

use crate::{GraphrtError, Runtime};

/// What kind of thing a lesson is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum LessonKind {
    /// How someone likes things done ("answer in Indonesian").
    Preference,
    /// Something that is true ("invoices go to finance").
    Fact,
    /// How to do something ("check stock before quoting").
    Procedure,
    /// A mistake not to repeat.
    Correction,
}

/// Where a lesson is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum LessonStatus {
    Proposed,
    Approved,
    Rejected,
    Retired,
}

/// A decision about a lesson.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum LessonDecision {
    /// Use it in runs.
    Approve,
    /// Decline it; the same text is not proposed again.
    Reject,
    /// Stop using an approved lesson, keeping it on record.
    Retire,
}

/// A lesson of an agent.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct LessonInfo {
    pub id: String,
    pub agent: String,
    /// The one user it applies to; `None` for every run of the agent.
    pub user: Option<String>,
    pub kind: LessonKind,
    pub text: String,
    pub status: LessonStatus,
    /// It could change what the agent does to the outside world (sending,
    /// money, credentials...): show it to the owner with care.
    pub high_risk: bool,
    /// Where it came from, as one English sentence.
    pub origin: String,
    pub created_at_ms: u64,
}

/// A lesson proposed from feedback on a run.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct LessonProposal {
    /// The agent (the `learning.agent` of its node).
    pub agent: String,
    /// The user of the run; the lesson applies to this user only.
    #[uniffi(default)]
    pub user: Option<String>,
    pub kind: LessonKind,
    pub text: String,
    /// The run the feedback is about.
    #[uniffi(default)]
    pub run_id: Option<String>,
    /// Who gave the feedback, and whether the app verified it is them (signed in).
    #[uniffi(default)]
    pub author: Option<String>,
    #[uniffi(default)]
    pub author_authenticated: bool,
    /// A short excerpt that shows why (mask secrets first).
    #[uniffi(default)]
    pub evidence: Option<String>,
}

#[cfg(feature = "learning")]
mod imp {
    use std::sync::Arc;

    use graphrt_learn::{Decision, LessonFilter, LessonStore, NewLesson, Proposal, Risk, Signal, UserFilter};

    use super::{LessonDecision, LessonInfo, LessonKind, LessonProposal, LessonStatus};
    use crate::GraphrtError;

    pub(crate) type Store = Arc<LessonStore>;

    /// Who the app is in the audit log.
    const ACTOR: &str = "app";

    fn failed(e: graphrt_learn::Error) -> GraphrtError {
        GraphrtError::input(e.to_string())
    }

    pub(crate) fn install(dir: &str, services: &mut graphrt_core::Services) -> Store {
        let store = Arc::new(LessonStore::open(dir));
        services.insert(Arc::clone(&store).into_service());
        store
    }

    fn kind_of(kind: LessonKind) -> graphrt_learn::LessonKind {
        match kind {
            LessonKind::Preference => graphrt_learn::LessonKind::Preference,
            LessonKind::Fact => graphrt_learn::LessonKind::Fact,
            LessonKind::Procedure => graphrt_learn::LessonKind::Procedure,
            LessonKind::Correction => graphrt_learn::LessonKind::Correction,
        }
    }

    fn status_of(status: LessonStatus) -> graphrt_learn::LessonStatus {
        match status {
            LessonStatus::Proposed => graphrt_learn::LessonStatus::Proposed,
            LessonStatus::Approved => graphrt_learn::LessonStatus::Approved,
            LessonStatus::Rejected => graphrt_learn::LessonStatus::Rejected,
            LessonStatus::Retired => graphrt_learn::LessonStatus::Retired,
        }
    }

    fn info(lesson: graphrt_learn::Lesson) -> LessonInfo {
        let origin = lesson.origin_sentence();
        LessonInfo {
            kind: match lesson.kind {
                graphrt_learn::LessonKind::Preference => LessonKind::Preference,
                graphrt_learn::LessonKind::Fact => LessonKind::Fact,
                graphrt_learn::LessonKind::Procedure => LessonKind::Procedure,
                graphrt_learn::LessonKind::Correction => LessonKind::Correction,
            },
            status: match lesson.status {
                graphrt_learn::LessonStatus::Proposed => LessonStatus::Proposed,
                graphrt_learn::LessonStatus::Approved => LessonStatus::Approved,
                graphrt_learn::LessonStatus::Rejected => LessonStatus::Rejected,
                graphrt_learn::LessonStatus::Retired => LessonStatus::Retired,
            },
            high_risk: lesson.risk == Risk::High,
            origin,
            id: lesson.id,
            agent: lesson.agent,
            user: lesson.user,
            text: lesson.text,
            created_at_ms: lesson.created_at_ms,
        }
    }

    pub(crate) fn list(
        store: &LessonStore,
        agent: &str,
        status: Option<LessonStatus>,
    ) -> Result<Vec<LessonInfo>, GraphrtError> {
        let filter = LessonFilter { user: UserFilter::Any, status: status.map(status_of) };
        Ok(store.list(agent, &filter).map_err(failed)?.into_iter().map(info).collect())
    }

    pub(crate) fn add(
        store: &LessonStore,
        agent: String,
        user: Option<String>,
        kind: LessonKind,
        text: String,
    ) -> Result<LessonInfo, GraphrtError> {
        let new = NewLesson { agent, user, kind: kind_of(kind), text };
        store.add(new, ACTOR).map(info).map_err(failed)
    }

    pub(crate) fn propose(store: &LessonStore, proposal: LessonProposal) -> Result<LessonInfo, GraphrtError> {
        let proposal = Proposal {
            agent: proposal.agent,
            subject_user: proposal.user,
            kind: kind_of(proposal.kind),
            text: proposal.text,
            signal: Signal::Feedback,
            run_id: proposal.run_id,
            author: proposal.author,
            author_authenticated: proposal.author_authenticated,
            evidence: proposal.evidence,
        };
        store.propose(proposal).map(info).map_err(failed)
    }

    pub(crate) fn decide(
        store: &LessonStore,
        agent: &str,
        id: &str,
        decision: LessonDecision,
    ) -> Result<LessonInfo, GraphrtError> {
        let decision = match decision {
            LessonDecision::Approve => Decision::Approve,
            LessonDecision::Reject => Decision::Reject,
            LessonDecision::Retire => Decision::Retire,
        };
        store.decide(agent, id, decision, ACTOR).map(info).map_err(failed)
    }

    pub(crate) fn delete(store: &LessonStore, agent: &str, id: &str) -> Result<(), GraphrtError> {
        store.delete(agent, id, ACTOR).map_err(failed)
    }
}

#[cfg(feature = "learning")]
pub(crate) use imp::{Store, install};

#[cfg(not(feature = "learning"))]
pub(crate) type Store = ();

impl Runtime {
    #[cfg(feature = "learning")]
    fn lesson_store(&self) -> Result<&Store, GraphrtError> {
        self.lessons.as_ref().ok_or_else(|| GraphrtError::not_configured("learning", "learning_dir"))
    }
}

#[cfg(not(feature = "learning"))]
macro_rules! without_learning {
    ($($unused:expr),*) => {{
        let _ = ($($unused,)*);
        Err(GraphrtError::unsupported("learning", "learning"))
    }};
}

#[uniffi::export]
impl Runtime {
    /// The lessons of `agent`, optionally only those in `status`.
    pub fn list_lessons(&self, agent: String, status: Option<LessonStatus>) -> Result<Vec<LessonInfo>, GraphrtError> {
        #[cfg(feature = "learning")]
        {
            imp::list(self.lesson_store()?, &agent, status)
        }
        #[cfg(not(feature = "learning"))]
        without_learning!(agent, status)
    }

    /// A lesson the owner wrote; it is approved at once.
    pub fn add_lesson(
        &self,
        agent: String,
        user: Option<String>,
        kind: LessonKind,
        text: String,
    ) -> Result<LessonInfo, GraphrtError> {
        #[cfg(feature = "learning")]
        {
            imp::add(self.lesson_store()?, agent, user, kind, text)
        }
        #[cfg(not(feature = "learning"))]
        without_learning!(agent, user, kind, text)
    }

    /// Propose a lesson from feedback on a run. It waits for the owner's decision
    /// ([`Runtime::decide_lesson`]) before any run uses it.
    pub fn propose_lesson(&self, proposal: LessonProposal) -> Result<LessonInfo, GraphrtError> {
        #[cfg(feature = "learning")]
        {
            imp::propose(self.lesson_store()?, proposal)
        }
        #[cfg(not(feature = "learning"))]
        without_learning!(proposal)
    }

    /// Approve, reject or retire a lesson.
    pub fn decide_lesson(
        &self,
        agent: String,
        id: String,
        decision: LessonDecision,
    ) -> Result<LessonInfo, GraphrtError> {
        #[cfg(feature = "learning")]
        {
            imp::decide(self.lesson_store()?, &agent, &id, decision)
        }
        #[cfg(not(feature = "learning"))]
        without_learning!(agent, id, decision)
    }

    /// Delete a lesson for good.
    pub fn delete_lesson(&self, agent: String, id: String) -> Result<(), GraphrtError> {
        #[cfg(feature = "learning")]
        {
            imp::delete(self.lesson_store()?, &agent, &id)
        }
        #[cfg(not(feature = "learning"))]
        without_learning!(agent, id)
    }
}
