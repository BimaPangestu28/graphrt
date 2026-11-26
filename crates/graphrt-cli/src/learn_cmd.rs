//! `graphrt learn`: review, correct and remove what agents learned (cargo
//! feature `learning`, preview).
//!
//! Every subcommand works on one lesson store: `--dir`, else
//! `$GRAPHRT_LEARNING_DIR`, else `~/.graphrt/learning`. `graphrt run` and
//! `graphrt serve` read the same default store for `agent` nodes with
//! `learning`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use graphrt_learn::{
    AuditEntry, Decision, Error as LearnError, Forget, Learner, Lesson, LessonFilter, LessonKind, LessonStatus,
    LessonStore, NewLesson, Observation, ObservationKind, Risk, Trust, UserFilter,
};
use serde_json::json;

use crate::args::{LearnCmdArgs, LearnCommand, LessonKindArg, LessonStatusArg};
use crate::error::{CliError, EXIT_OK};

/// Execute a `graphrt learn` subcommand.
pub async fn learn(args: LearnCmdArgs) -> Result<u8, CliError> {
    let store = Arc::new(open_store(args.dir)?);
    let actor = args.actor.as_str();
    let json = args.json;
    let mut out = std::io::stdout().lock();
    match args.command {
        LearnCommand::List { agent, user, shared, status } => {
            let user = match (user, shared) {
                (Some(user), _) => UserFilter::User(user),
                (None, true) => UserFilter::AgentWide,
                (None, false) => UserFilter::Any,
            };
            let filter = LessonFilter { user, status: status.map(status_of) };
            let lessons = store.list(&agent, &filter).map_err(failed)?;
            print_lessons(&lessons, json, &mut out)
        }
        LearnCommand::Add { agent, text, user, kind } => {
            let lesson = store.add(NewLesson { agent, user, kind: kind_of(kind), text }, actor).map_err(failed)?;
            print_lesson(&lesson, json, &mut out)
        }
        LearnCommand::Approve { agent, id } => decide(&store, &agent, &id, Decision::Approve, actor, json, &mut out),
        LearnCommand::Reject { agent, id } => decide(&store, &agent, &id, Decision::Reject, actor, json, &mut out),
        LearnCommand::Retire { agent, id } => decide(&store, &agent, &id, Decision::Retire, actor, json, &mut out),
        LearnCommand::Edit { agent, id, text } => {
            let lesson = store.edit(&agent, &id, &text, actor).map_err(failed)?;
            print_lesson(&lesson, json, &mut out)
        }
        LearnCommand::Promote { agent, id } => {
            let lesson = store.promote(&agent, &id, actor).map_err(failed)?;
            print_lesson(&lesson, json, &mut out)
        }
        LearnCommand::Delete { agent, id } => {
            store.delete(&agent, &id, actor).map_err(failed)?;
            print_done(json!({"deleted": id}), &format!("deleted lesson {id}"), json, &mut out)
        }
        LearnCommand::Forget { agent, user, run } => {
            let what = match (user, run) {
                (Some(user), _) => Forget::User(user),
                (None, Some(run)) => Forget::Run(run),
                (None, None) => return Err(CliError::usage("pass `--user <USER>` or `--run <RUN_ID>`")),
            };
            let removed = store.forget(&agent, &what, actor).map_err(failed)?;
            print_done(json!({"removed": removed}), &format!("removed {removed} lesson(s)"), json, &mut out)
        }
        LearnCommand::Feedback { agent, run, comment, user, authenticated } => {
            let observation = Observation {
                agent,
                user: user.clone(),
                run_id: run,
                kind: ObservationKind::Feedback { value: None, comment: Some(comment), author: user, authenticated },
                excerpt: None,
                at_ms: 0,
            };
            let recorded = Learner::new(Arc::clone(&store)).observe(observation).map_err(failed)?;
            let text = if recorded {
                "feedback recorded; run `graphrt learn distill <AGENT>` to turn it into a proposal"
            } else {
                "feedback dropped: the queue is full; run `graphrt learn distill <AGENT>` first"
            };
            print_done(json!({"recorded": recorded}), text, json, &mut out)
        }
        LearnCommand::Distill { agent } => {
            let report = Learner::new(Arc::clone(&store)).distill(&agent, None).await.map_err(failed)?;
            if json {
                let value = json!({
                    "observations": report.observations, "lessons": report.lessons, "skipped": report.skipped,
                });
                return print_json(&value, &mut out);
            }
            writeln!(out, "read {} signal(s), recorded {} lesson(s)", report.observations, report.lessons.len())
                .map_err(io_error)?;
            for skipped in &report.skipped {
                writeln!(out, "  skipped: {skipped}").map_err(io_error)?;
            }
            print_lessons(&report.lessons, false, &mut out)
        }
        LearnCommand::Policy { agent, auto_approve } => {
            let mut policy = store.policy(&agent).map_err(failed)?;
            if let Some(auto_approve) = auto_approve {
                policy.auto_approve_user_feedback = auto_approve;
                store.set_policy(&agent, policy, actor).map_err(failed)?;
            }
            if json {
                return print_json(&policy, &mut out);
            }
            let auto = if policy.auto_approve_user_feedback {
                "on: low risk feedback of an authenticated user is approved for that user's own runs"
            } else {
                "off: every lesson waits for the owner"
            };
            writeln!(out, "automatic approval: {auto}").map_err(io_error)?;
            writeln!(
                out,
                "limits: {} lessons per scope, {} open proposals, {} characters per run, proposals lapse after {} days",
                policy.max_lessons_per_scope,
                policy.max_open_proposals,
                policy.max_note_chars,
                policy.proposal_ttl_days
            )
            .map_err(io_error)?;
            Ok(EXIT_OK)
        }
        LearnCommand::Audit { agent } => {
            let entries = store.audit(&agent).map_err(failed)?;
            print_audit(&entries, json, &mut out)
        }
    }
}

fn open_store(dir: Option<PathBuf>) -> Result<LessonStore, CliError> {
    let root = dir.or_else(LessonStore::default_root).ok_or_else(|| {
        CliError::usage(
            "cannot find a home directory for the lesson store; pass `--dir <DIR>` or set GRAPHRT_LEARNING_DIR",
        )
    })?;
    Ok(LessonStore::open(root))
}

fn status_of(status: LessonStatusArg) -> LessonStatus {
    match status {
        LessonStatusArg::Proposed => LessonStatus::Proposed,
        LessonStatusArg::Approved => LessonStatus::Approved,
        LessonStatusArg::Rejected => LessonStatus::Rejected,
        LessonStatusArg::Retired => LessonStatus::Retired,
    }
}

fn kind_of(kind: LessonKindArg) -> LessonKind {
    match kind {
        LessonKindArg::Preference => LessonKind::Preference,
        LessonKindArg::Fact => LessonKind::Fact,
        LessonKindArg::Procedure => LessonKind::Procedure,
        LessonKindArg::Correction => LessonKind::Correction,
    }
}

fn io_error(error: std::io::Error) -> CliError {
    CliError::failed(error.to_string())
}

fn failed(error: LearnError) -> CliError {
    match error {
        LearnError::Io { .. } | LearnError::Damaged(_) => CliError::failed(error.to_string()),
        other => CliError::usage(other.to_string()),
    }
}

fn print_json(value: &impl serde::Serialize, out: &mut impl Write) -> Result<u8, CliError> {
    let text = serde_json::to_string_pretty(value).map_err(|e| CliError::failed(e.to_string()))?;
    writeln!(out, "{text}").map_err(io_error)?;
    Ok(EXIT_OK)
}

fn print_done(value: serde_json::Value, text: &str, json: bool, out: &mut impl Write) -> Result<u8, CliError> {
    if json {
        return print_json(&value, out);
    }
    writeln!(out, "{text}").map_err(io_error)?;
    Ok(EXIT_OK)
}

fn decide(
    store: &LessonStore,
    agent: &str,
    id: &str,
    decision: Decision,
    actor: &str,
    json: bool,
    out: &mut impl Write,
) -> Result<u8, CliError> {
    let lesson = store.decide(agent, id, decision, actor).map_err(failed)?;
    print_lesson(&lesson, json, out)
}

fn print_lesson(lesson: &Lesson, json: bool, out: &mut impl Write) -> Result<u8, CliError> {
    if json {
        return print_json(lesson, out);
    }
    write_lesson(lesson, out)?;
    Ok(EXIT_OK)
}

fn print_lessons(lessons: &[Lesson], json: bool, out: &mut impl Write) -> Result<u8, CliError> {
    if json {
        return print_json(&lessons, out);
    }
    if lessons.is_empty() {
        writeln!(out, "no lessons").map_err(io_error)?;
    }
    for lesson in lessons {
        write_lesson(lesson, out)?;
    }
    Ok(EXIT_OK)
}

fn write_lesson(lesson: &Lesson, out: &mut impl Write) -> Result<(), CliError> {
    let status = match lesson.status {
        LessonStatus::Proposed => "proposed",
        LessonStatus::Approved => "approved",
        LessonStatus::Rejected => "rejected",
        LessonStatus::Retired => "retired",
    };
    let applies_to = lesson.user.as_deref().map_or_else(|| "every run".to_owned(), |user| format!("user {user}"));
    writeln!(out, "{}  [{status}]  {}", lesson.id, lesson.text).map_err(io_error)?;
    writeln!(out, "    applies to: {applies_to}").map_err(io_error)?;
    let trust = match lesson.trust {
        Trust::Owner => "the owner's words",
        Trust::User => "a verified user's own words",
        Trust::Derived => "derived from run content, not verified",
    };
    writeln!(out, "    source: {} ({trust})", lesson.origin_sentence()).map_err(io_error)?;
    if let Some(evidence) = &lesson.origin.evidence {
        writeln!(out, "    evidence: {evidence}").map_err(io_error)?;
    }
    if lesson.risk == Risk::High {
        let reasons: Vec<&str> = lesson.risk_reasons.iter().map(|reason| reason.message.as_str()).collect();
        writeln!(out, "    needs a careful look: {}", reasons.join(" ")).map_err(io_error)?;
    }
    Ok(())
}

fn print_audit(entries: &[AuditEntry], json: bool, out: &mut impl Write) -> Result<u8, CliError> {
    if json {
        return print_json(&entries, out);
    }
    for entry in entries {
        let lesson = entry.lesson_id.as_deref().unwrap_or("-");
        let detail = entry.detail.as_deref().unwrap_or_default();
        writeln!(out, "{}  {:12}  {:20}  {}  {detail}", entry.at_ms, entry.action, lesson, entry.actor)
            .map_err(io_error)?;
    }
    Ok(EXIT_OK)
}
