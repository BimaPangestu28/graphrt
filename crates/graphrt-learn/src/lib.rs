//! # graphrt-learn
//!
//! Self learning for graphrt agents. An agent gets better from its own use:
//! corrections from users and patterns from earlier runs become **lessons**,
//! short statements that later runs read.
//!
//! - [`lesson`]: what a lesson is, where it came from, how far to trust it.
//! - [`policy`]: limits, and the one case where a lesson may be approved
//!   without a person.
//! - [`store`]: lessons on disk, per agent and per user, with an audit log.
//! - [`learner`]: recording signals cheaply and turning them into proposals
//!   outside the run.
//! - [`model_distiller`]: a model that suggests lessons from recorded signals.
//! - [`skill_draft`]: a skill draft from the procedures an agent learned.
//!
//! The rules that make this safe are enforced by the store:
//!
//! - **Only approved lessons reach a run.**
//! - **A lesson derived from run content never approves itself.** Text an
//!   agent read or produced can become a proposal and nothing more.
//! - **Scope is exact.** A run reads the lessons of its agent and of its own
//!   user. A lesson that came from one user stays with that user until the
//!   owner promotes it.
//! - **Everything can be seen, changed and removed**, by lesson, by user or
//!   by run.
//!
//! ```no_run
//! use std::sync::Arc;
//! use graphrt_learn::{Decision, LessonFilter, LessonStatus, LessonStore};
//!
//! # fn main() -> Result<(), graphrt_learn::Error> {
//! let store = Arc::new(LessonStore::open("/var/lib/graphrt/learning"));
//! let filter = LessonFilter { status: Some(LessonStatus::Proposed), ..LessonFilter::default() };
//! for lesson in store.list("support-bot", &filter)? {
//!     println!("{} ({})", lesson.text, lesson.origin_sentence());
//!     store.decide("support-bot", &lesson.id, Decision::Approve, "owner")?;
//! }
//! // Register with the engine's services so `agent` nodes with `learning` read the lessons.
//! let service = store.into_service();
//! # let _ = service;
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod learner;
pub mod lesson;
pub mod model_distiller;
pub mod policy;
pub mod skill_draft;
pub mod store;

pub use error::Error;
pub use learner::{Candidate, DistillReport, Distiller, Learner, Observation, ObservationKind};
pub use lesson::{Lesson, LessonKind, LessonStatus, Origin, Risk, RiskReason, Signal, Trust};
pub use model_distiller::ModelDistiller;
pub use policy::LearningPolicy;
pub use skill_draft::{draft_skill_from_lessons, procedure_lessons};
pub use store::{AuditEntry, Decision, Forget, LessonFilter, LessonStore, NewLesson, Proposal, UserFilter};
