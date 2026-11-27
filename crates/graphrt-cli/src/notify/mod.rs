//! Notifications from `graphrt serve` when a run needs a person (cargo feature
//! `notify`): a run failed, or it waits for an approval or an answer.
//!
//! A [`Notifier`] is one way to reach people. The server ships Web Push
//! ([`webpush`], feature `web-push`); a chat bridge (Telegram, Slack, Teams) is
//! another implementation of the same trait, added to the same
//! [`Notifications`].
//!
//! Sending never delays or fails a run: each notice is sent from its own task,
//! and what goes wrong is logged.

#[cfg(feature = "web-push")]
pub(crate) mod push_api;
#[cfg(feature = "web-push")]
pub mod webpush;

use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::Value;
use graphrt_core::store::{RunRecord, RunStatus};

/// The longest notice text, in characters. Push services cap a message at 4096
/// bytes, encrypted, and a phone shows two or three lines.
const MAX_BODY_CHARS: usize = 180;

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    /// A run paused for an approval or an answer.
    ApprovalNeeded,
    /// A run ended with an error.
    RunFailed,
}

impl NoticeKind {
    /// The kind as payloads write it.
    pub fn name(self) -> &'static str {
        match self {
            NoticeKind::ApprovalNeeded => "approval_needed",
            NoticeKind::RunFailed => "run_failed",
        }
    }
}

/// One thing to tell people.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub run_id: String,
    pub workflow: String,
    /// One line, e.g. `Approval needed: refunds`.
    pub title: String,
    /// What is asked or what went wrong, shortened.
    pub body: String,
}

/// Why a notice was not delivered.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct NotifyError(pub String);

/// One way to reach people.
pub trait Notifier: Send + Sync + std::fmt::Debug {
    /// A short name for logs, e.g. `web-push`.
    fn name(&self) -> &'static str;

    /// Deliver `notice` to everyone this notifier reaches.
    ///
    /// @throws NotifyError - When nobody could be reached. Partial failures are the notifier's to log.
    fn notify<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<(), NotifyError>>;
}

/// The notifiers of a server.
#[derive(Debug, Default)]
pub struct Notifications {
    notifiers: Vec<Arc<dyn Notifier>>,
    /// Say nothing about pauses of runs a verified user started: with
    /// `--user-resume` those are questions to that user, who is looking at them.
    quiet_user_pauses: bool,
}

impl Notifications {
    pub fn new(notifiers: Vec<Arc<dyn Notifier>>, quiet_user_pauses: bool) -> Self {
        Self { notifiers, quiet_user_pauses }
    }

    pub fn is_empty(&self) -> bool {
        self.notifiers.is_empty()
    }

    /// Tell people about the end of an execution of `record`, when it is one
    /// they should know of. Returns at once: delivery runs in the background.
    pub fn run_ended(&self, record: &RunRecord) {
        let Some(notice) = self.notice_for(record) else { return };
        for notifier in &self.notifiers {
            let notifier = Arc::clone(notifier);
            let notice = notice.clone();
            graphrt_core::task::spawn(async move {
                if let Err(error) = notifier.notify(&notice).await {
                    eprintln!("warning: run {}: {} notification failed: {error}", notice.run_id, notifier.name());
                }
            });
        }
    }

    /// The notice for the state `record` ended an execution in, if any.
    pub fn notice_for(&self, record: &RunRecord) -> Option<Notice> {
        if self.quiet_user_pauses && record.user_verified && record.status == RunStatus::Paused {
            return None;
        }
        notice_for(record)
    }
}

/// `text` in one line of at most [`MAX_BODY_CHARS`] characters.
fn shorten(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= MAX_BODY_CHARS {
        return line;
    }
    let mut short: String = line.chars().take(MAX_BODY_CHARS - 1).collect();
    short.push('…');
    short
}

/// What a paused run asks, in words: the first prompt, or the tools it wants to call.
fn question_of(record: &RunRecord) -> Option<String> {
    let waiting = record.pause_reasons.iter().filter(|reason| reason.node_id.is_some());
    let mut first = None;
    for reason in waiting {
        if let Some(prompt) = reason.data.get_path(&["prompt"]).as_ref().and_then(Value::as_str)
            && !prompt.trim().is_empty()
        {
            return Some(prompt.to_owned());
        }
        if let Some(serde_json::Value::Array(calls)) = reason.data.get_path(&["calls"]).map(|calls| calls.to_json()) {
            let names: Vec<&str> =
                calls.iter().filter_map(|call| call.get("name").and_then(serde_json::Value::as_str)).collect();
            if !names.is_empty() {
                return Some(format!("Wants to call {}", names.join(", ")));
            }
        }
        first.get_or_insert_with(|| "A step waits for an answer".to_owned());
    }
    first
}

/// The notice for the state `record` is in: a failure, or a pause that waits
/// for a person. Success, an abort and a shutdown drain need nobody.
pub fn notice_for(record: &RunRecord) -> Option<Notice> {
    let (kind, title, body) = match record.status {
        RunStatus::Failed => (
            NoticeKind::RunFailed,
            format!("Run failed: {}", record.workflow),
            record.error.clone().unwrap_or_else(|| "The run ended with an error".to_owned()),
        ),
        RunStatus::Paused => {
            (NoticeKind::ApprovalNeeded, format!("Approval needed: {}", record.workflow), question_of(record)?)
        }
        _ => return None,
    };
    Some(Notice {
        kind,
        run_id: record.run_id.clone(),
        workflow: record.workflow.clone(),
        title: shorten(&title),
        body: shorten(&body),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Mutex;

    use graphrt_core::event::PauseReason;
    use serde_json::json;

    use super::*;

    fn record(status: RunStatus) -> RunRecord {
        let mut record = RunRecord::running("run-1", "refunds", "fp", 1);
        record.status = status;
        record
    }

    fn waits_for(kind: &str, node: Option<&str>, data: serde_json::Value) -> RunRecord {
        let mut record = record(RunStatus::Paused);
        record.pause_reasons = vec![PauseReason {
            frame: Arc::from(""),
            node_id: node.map(str::to_owned),
            kind: kind.to_owned(),
            data: Value::from_json(data),
        }];
        record
    }

    #[test]
    fn a_failure_and_a_question_are_worth_a_notice_and_nothing_else_is() {
        let mut failed = record(RunStatus::Failed);
        failed.error = Some("model: 529\n overloaded".to_owned());
        let notice = notice_for(&failed).unwrap();
        assert_eq!((notice.kind, notice.title.as_str()), (NoticeKind::RunFailed, "Run failed: refunds"));
        assert_eq!(notice.body, "model: 529 overloaded");

        let asked = waits_for("human_input", Some("ask"), json!({ "prompt": "Refund 40 EUR to Ana?" }));
        let notice = notice_for(&asked).unwrap();
        assert_eq!(notice.kind, NoticeKind::ApprovalNeeded);
        assert_eq!(notice.body, "Refund 40 EUR to Ana?");

        let tools = json!({ "calls": [{ "name": "orders/refund" }, { "name": "mail/send" }] });
        assert_eq!(
            notice_for(&waits_for("tool_approval", Some("agent"), tools)).unwrap().body,
            "Wants to call orders/refund, mail/send"
        );
        assert_eq!(
            notice_for(&waits_for("custom", Some("node"), json!({}))).unwrap().body,
            "A step waits for an answer"
        );

        // A drain waits for no person; a success and an abort need nobody.
        assert_eq!(notice_for(&waits_for("drained", None, json!({ "resume": "auto" }))), None);
        assert_eq!(notice_for(&record(RunStatus::Succeeded)), None);
        assert_eq!(notice_for(&record(RunStatus::Aborted)), None);
    }

    #[test]
    fn long_text_is_shortened_to_one_line() {
        let mut failed = record(RunStatus::Failed);
        failed.error = Some("x".repeat(1000));
        let body = notice_for(&failed).unwrap().body;
        assert_eq!(body.chars().count(), MAX_BODY_CHARS);
        assert!(body.ends_with('…'));
    }

    #[test]
    fn questions_to_the_user_who_started_the_run_are_not_announced_when_users_answer_themselves() {
        let mut asked = waits_for("human_input", Some("ask"), json!({ "prompt": "Which size?" }));
        asked.user_verified = true;
        asked.user_id = Some("alice".to_owned());
        assert!(Notifications::new(Vec::new(), false).notice_for(&asked).is_some());
        let quiet = Notifications::new(Vec::new(), true);
        assert!(quiet.notice_for(&asked).is_none());
        // A failure of that user's run is still announced.
        asked.status = RunStatus::Failed;
        assert!(quiet.notice_for(&asked).is_some());
    }

    #[derive(Debug, Default)]
    struct Recording(Mutex<Vec<String>>);

    impl Notifier for Recording {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn notify<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<(), NotifyError>> {
            Box::pin(async move {
                self.0.lock().unwrap().push(notice.title.clone());
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn every_notifier_gets_the_notice() {
        let (first, second) = (Arc::new(Recording::default()), Arc::new(Recording::default()));
        let notifiers: Vec<Arc<dyn Notifier>> = vec![first.clone(), second.clone()];
        let notifications = Notifications::new(notifiers, false);
        notifications.run_ended(&record(RunStatus::Failed));
        notifications.run_ended(&record(RunStatus::Succeeded));
        for _ in 0..200 {
            if !first.0.lock().unwrap().is_empty() && !second.0.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(*first.0.lock().unwrap(), ["Run failed: refunds"]);
        assert_eq!(*second.0.lock().unwrap(), ["Run failed: refunds"]);
    }
}
