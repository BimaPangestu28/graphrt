//! Turning what happened in runs into lesson proposals, outside the run.
//!
//! Two steps, kept apart on purpose:
//!
//! 1. [`Learner::observe`] records a signal (feedback on a run, how a run
//!    ended). It appends one line to a file and does nothing else, so a host
//!    can call it on the request path.
//! 2. [`Learner::distill`] reads the recorded signals and turns them into
//!    proposals. A host runs it when it likes: from a command, on a timer,
//!    when the app is idle. It never runs inside an agent's run.
//!
//! What becomes a proposal:
//!
//! - **Feedback with a comment** becomes a proposal with the comment as its
//!   text. It is the person's own words, so its trust is
//!   [`Trust::User`](crate::lesson::Trust::User) when the host authenticated
//!   them, and [`Trust::Derived`](crate::lesson::Trust::Derived) otherwise.
//! - **Everything else** (failed runs, feedback without a comment, run
//!   content) is handed to a [`Distiller`], when the host supplies one. A
//!   distiller is usually a model. Whatever it returns is
//!   [`Trust::Derived`](crate::lesson::Trust::Derived): it was produced from
//!   content nobody vouched for, so it always waits for the owner.

use std::io::Write;
use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::private_fs;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::lesson::{Lesson, LessonKind, Signal, check_agent, check_user};
use crate::store::{LessonStore, Proposal, now_ms};

const OBSERVATIONS_FILE: &str = "observations.jsonl";
/// The queue stops growing at this size. Later signals are dropped until it is distilled.
pub const MAX_QUEUE_BYTES: u64 = 1024 * 1024;
/// Longest feedback comment or run excerpt kept in the queue, in characters.
pub const MAX_OBSERVATION_CHARS: usize = 2_000;
/// Most candidates taken from one [`Distiller`] call.
pub const MAX_CANDIDATES: usize = 20;

/// What was observed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ObservationKind {
    /// A person rated or commented on a run.
    Feedback {
        /// A score, when one was given. Below zero or `false` style values mean "bad".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<f64>,
        /// What the person wrote.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comment: Option<String>,
        /// Who gave it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        author: Option<String>,
        /// Whether the host verified who the author is.
        #[serde(default)]
        authenticated: bool,
    },
    /// A run failed.
    RunFailed {
        /// The error, with secrets already masked.
        error: String,
    },
    /// A run finished.
    RunSucceeded,
}

/// One recorded signal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// The agent the run belonged to.
    pub agent: String,
    /// The user the run was for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The run.
    pub run_id: String,
    /// What was observed.
    #[serde(flatten)]
    pub kind: ObservationKind,
    /// A short excerpt of the run (the request and the answer), with secrets
    /// already masked. Run content is untrusted: it is only ever shown to a
    /// person as evidence or handed to a [`Distiller`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    /// When it was recorded, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub at_ms: u64,
}

/// A lesson a [`Distiller`] suggests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// What kind of thing it is.
    pub kind: LessonKind,
    /// The lesson.
    pub text: String,
    /// The run it was drawn from, when it is one of the observed runs.
    pub run_id: Option<String>,
}

/// Something that reads observations and suggests lessons: usually a model
/// with a reflection prompt. Its output is never trusted.
pub trait Distiller: Send + Sync {
    /// Suggest lessons for `agent` from `observations`.
    fn distill<'a>(
        &'a self,
        agent: &'a str,
        observations: &'a [Observation],
    ) -> BoxFuture<'a, Result<Vec<Candidate>, String>>;
}

/// What one [`Learner::distill`] pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DistillReport {
    /// Signals read from the queue.
    pub observations: usize,
    /// Lessons proposed or, where the policy allows, approved.
    pub lessons: Vec<Lesson>,
    /// Suggestions that were not recorded, with the reason.
    pub skipped: Vec<String>,
}

/// Records signals and turns them into proposals.
#[derive(Clone, Debug)]
pub struct Learner {
    store: Arc<LessonStore>,
}

impl Learner {
    /// A learner that writes proposals to `store`.
    pub fn new(store: Arc<LessonStore>) -> Self {
        Self { store }
    }

    /// Record a signal. Cheap: one appended line, no model call.
    ///
    /// Returns `false` when the signal was dropped because the queue is full.
    ///
    /// # Errors
    /// [`Error::InvalidAgent`], [`Error::InvalidUser`], or a write error.
    pub fn observe(&self, mut observation: Observation) -> Result<bool, Error> {
        check_agent(&observation.agent)?;
        if let Some(user) = &observation.user {
            check_user(user)?;
        }
        let dir = self.store.agent_dir(&observation.agent);
        let path = dir.join(OBSERVATIONS_FILE);
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() >= MAX_QUEUE_BYTES) {
            return Ok(false);
        }
        observation.at_ms = now_ms();
        observation.excerpt = observation.excerpt.as_deref().map(cut);
        match &mut observation.kind {
            ObservationKind::Feedback { comment, .. } => *comment = comment.as_deref().map(cut),
            ObservationKind::RunFailed { error } => *error = cut(error),
            ObservationKind::RunSucceeded => {}
        }
        let mut line = serde_json::to_string(&observation).map_err(|e| Error::Damaged(e.to_string()))?;
        line.push('\n');
        private_fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, &e))?;
        let mut file = private_fs::append_file(&path).map_err(|e| Error::io(&path, &e))?;
        file.write_all(line.as_bytes()).map_err(|e| Error::io(&path, &e))?;
        Ok(true)
    }

    /// The signals waiting to be distilled, oldest first.
    ///
    /// # Errors
    /// [`Error::InvalidAgent`], or a read error.
    pub fn pending(&self, agent: &str) -> Result<Vec<Observation>, Error> {
        check_agent(agent)?;
        let path = self.store.agent_dir(agent).join(OBSERVATIONS_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Error::io(path, &error)),
        };
        // A line that does not parse (a torn write) is skipped, not fatal.
        Ok(text
            .lines()
            .filter_map(|line| serde_json::from_str::<Observation>(line).ok())
            .filter(|observation| observation.agent == agent)
            .collect())
    }

    /// Turn the waiting signals of `agent` into proposals and empty the queue.
    ///
    /// Feedback comments become proposals directly. With a `distiller`, the
    /// signals are also handed to it and each suggestion becomes a proposal
    /// with [`Trust::Derived`](crate::lesson::Trust::Derived). A failing
    /// distiller is reported in the result and does not lose the comments.
    ///
    /// # Errors
    /// [`Error::InvalidAgent`], or a read or write error. A single suggestion
    /// that cannot be recorded is listed in [`DistillReport::skipped`].
    pub async fn distill(&self, agent: &str, distiller: Option<&dyn Distiller>) -> Result<DistillReport, Error> {
        let observations = self.pending(agent)?;
        let mut report = DistillReport { observations: observations.len(), ..DistillReport::default() };
        if observations.is_empty() {
            return Ok(report);
        }
        for observation in &observations {
            let ObservationKind::Feedback { comment: Some(comment), author, authenticated, .. } = &observation.kind
            else {
                continue;
            };
            if comment.trim().is_empty() {
                continue;
            }
            let proposal = Proposal {
                agent: agent.to_owned(),
                subject_user: observation.user.clone(),
                kind: LessonKind::Correction,
                text: comment.clone(),
                signal: Signal::Feedback,
                run_id: Some(observation.run_id.clone()),
                author: author.clone(),
                author_authenticated: *authenticated,
                evidence: observation.excerpt.clone(),
            };
            self.record(proposal, &mut report);
        }
        if let Some(distiller) = distiller {
            match distiller.distill(agent, &observations).await {
                Ok(candidates) => {
                    for candidate in candidates.into_iter().take(MAX_CANDIDATES) {
                        // A suggestion is tied to the user of the run it names. One that
                        // names no observed run applies to nobody in particular, and
                        // still waits for the owner like every derived lesson.
                        let source = candidate
                            .run_id
                            .as_deref()
                            .and_then(|run| observations.iter().find(|observation| observation.run_id == run));
                        let proposal = Proposal {
                            agent: agent.to_owned(),
                            subject_user: source.and_then(|observation| observation.user.clone()),
                            kind: candidate.kind,
                            text: candidate.text,
                            signal: Signal::Reflection,
                            run_id: source.map(|observation| observation.run_id.clone()),
                            author: None,
                            author_authenticated: false,
                            evidence: source.and_then(|observation| observation.excerpt.clone()),
                        };
                        self.record(proposal, &mut report);
                    }
                }
                Err(message) => report.skipped.push(format!("the distiller failed: {}", cut(&message))),
            }
        }
        self.clear(agent)?;
        Ok(report)
    }

    fn record(&self, proposal: Proposal, report: &mut DistillReport) {
        match self.store.propose(proposal) {
            Ok(lesson) => {
                if !report.lessons.iter().any(|known| known.id == lesson.id) {
                    report.lessons.push(lesson);
                }
            }
            Err(error) => report.skipped.push(error.to_string()),
        }
    }

    fn clear(&self, agent: &str) -> Result<(), Error> {
        let path = self.store.agent_dir(agent).join(OBSERVATIONS_FILE);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::io(path, &error)),
        }
    }
}

/// Text on one line, visible characters only, cut to [`MAX_OBSERVATION_CHARS`].
fn cut(text: &str) -> String {
    use graphrt_skill::text::{hidden_character, shorten, single_line};
    let visible: String = text.chars().filter(|c| hidden_character(c.encode_utf8(&mut [0; 4])).is_none()).collect();
    shorten(&single_line(&visible), MAX_OBSERVATION_CHARS)
}
