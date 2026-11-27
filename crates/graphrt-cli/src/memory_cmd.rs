//! `graphrt memory`: review, correct and erase what agents remember (cargo
//! feature `memory`).
//!
//! Every subcommand works on one memory store: `--dir`, else
//! `$GRAPHRT_MEMORY_DIR`, else `~/.graphrt/memory`, under `--tenant` when the
//! server runs with `--memory-tenant`. `graphrt run` and `graphrt serve` use
//! the same default store for `agent` nodes with `long_term_memory`.

use std::io::Write;
use std::path::PathBuf;

use graphrt_core::memory::{
    ForgetTarget, MemoryBank, MemoryError, MemoryRecord, MemoryScope, MemorySource, MemoryStatus, MemoryTrust,
    NewMemory, WritePolicy,
};
use graphrt_memory::{ListFilter, MemoryStore};
use serde_json::json;

use crate::args::{MemoryCmdArgs, MemoryCommand};
use crate::error::{CliError, EXIT_OK};

fn failed(error: MemoryError) -> CliError {
    match error.kind {
        "memory_scope" | "memory_invalid" | "memory_too_long" | "memory_secret" => CliError::usage(error.message),
        _ => CliError::failed(error.message),
    }
}

fn write_out(out: &mut impl Write, text: &str) -> Result<(), CliError> {
    writeln!(out, "{text}").map_err(|e| CliError::failed(format!("cannot write the output: {e}")))
}

fn print_notes(notes: &[MemoryRecord], json: bool, out: &mut impl Write) -> Result<u8, CliError> {
    if json {
        write_out(out, &serde_json::to_string_pretty(&json!({ "notes": notes })).unwrap_or_default())?;
        return Ok(EXIT_OK);
    }
    if notes.is_empty() {
        write_out(out, "no notes")?;
    }
    for note in notes {
        let line = format!(
            "{}  {:<7} {:<10} {:<16} {}",
            note.id,
            note.status.as_str(),
            note.kind.as_str(),
            note.trust.as_str(),
            note.text
        );
        write_out(out, &line)?;
    }
    Ok(EXIT_OK)
}

fn print_done(value: serde_json::Value, text: &str, json: bool, out: &mut impl Write) -> Result<u8, CliError> {
    write_out(out, &if json { value.to_string() } else { text.to_owned() })?;
    Ok(EXIT_OK)
}

/// Execute a `graphrt memory` subcommand.
pub async fn memory(args: MemoryCmdArgs) -> Result<u8, CliError> {
    if let Some(tenant) = &args.tenant {
        graphrt_memory::check_name("tenant", tenant).map_err(failed)?;
    }
    let store = MemoryStore::open(args.dir.clone().unwrap_or_else(MemoryStore::default_root));
    let scope =
        |agent: &str, user: Option<String>| MemoryScope { tenant: args.tenant.clone(), agent: agent.to_owned(), user };
    let source = MemorySource { actor: format!("owner:{}", args.actor), ..MemorySource::default() };
    let json = args.json;
    let mut out = std::io::stdout().lock();
    match &args.command {
        MemoryCommand::List { agent, user, pending, query } => {
            let filter = ListFilter { status: pending.then_some(MemoryStatus::Pending), query: query.clone() };
            let notes = store.list(&scope(agent, user.clone()), &filter).await.map_err(failed)?;
            print_notes(&notes, json, &mut out)
        }
        MemoryCommand::Add { agent, text, user } => {
            let note = NewMemory::new(text.clone(), MemoryTrust::Owner, source).policy(WritePolicy::Open);
            let record = store.remember(&scope(agent, user.clone()), note).await.map_err(failed)?;
            print_notes(std::slice::from_ref(&record), json, &mut out)
        }
        MemoryCommand::Approve { agent, id, user } | MemoryCommand::Reject { agent, id, user } => {
            let approve = matches!(args.command, MemoryCommand::Approve { .. });
            let decided = store.decide(&scope(agent, user.clone()), id, approve, &source).await.map_err(failed)?;
            match decided {
                None => Err(CliError::usage(format!("no note `{id}` in this scope (pass --user for a user's note)"))),
                Some(_) if !approve => {
                    print_done(json!({"removed": id}), &format!("rejected and removed {id}"), json, &mut out)
                }
                Some(record) => print_notes(std::slice::from_ref(&record), json, &mut out),
            }
        }
        MemoryCommand::Forget { agent, id, user } => {
            let removed = store
                .forget(&scope(agent, user.clone()), &ForgetTarget::Id(id.clone()), &source)
                .await
                .map_err(failed)?;
            if removed == 0 {
                return Err(CliError::usage(format!("no note `{id}` in this scope (pass --user for a user's note)")));
            }
            print_done(json!({"removed": removed}), &format!("removed {id}"), json, &mut out)
        }
        MemoryCommand::Export { agent, user } => {
            let export = store.export_user(&scope(agent, Some(user.clone()))).await.map_err(failed)?;
            print_done(export.clone(), &serde_json::to_string_pretty(&export).unwrap_or_default(), true, &mut out)
        }
        MemoryCommand::ForgetUser { agent, user } => {
            let notes = store.erase_user(&scope(agent, Some(user.clone())), &source).await.map_err(failed)?;
            let lessons = forget_lessons(args.learning_dir.clone(), agent, user, &source.actor)?;
            let said = match lessons {
                Some(lessons) => format!("erased {notes} note(s) and {lessons} lesson(s) of that user"),
                None => format!("erased {notes} note(s) of that user (no lesson store found to erase lessons from)"),
            };
            print_done(json!({"notes": notes, "lessons": lessons}), &said, json, &mut out)
        }
        MemoryCommand::Audit { agent, limit } => {
            let entries = store.audit(args.tenant.as_deref(), agent, *limit).await.map_err(failed)?;
            for entry in &entries {
                write_out(&mut out, &entry.to_string())?;
            }
            Ok(EXIT_OK)
        }
    }
}

/// Erase the lessons of the same user, so erasing a user leaves nothing of
/// them in either store. `None` when there is no lesson store to look in.
#[cfg(feature = "learning")]
fn forget_lessons(dir: Option<PathBuf>, agent: &str, user: &str, actor: &str) -> Result<Option<usize>, CliError> {
    let Some(root) = dir.or_else(graphrt_learn::LessonStore::default_root) else { return Ok(None) };
    match graphrt_learn::LessonStore::open(root).forget(agent, &graphrt_learn::Forget::User(user.to_owned()), actor) {
        Ok(removed) => Ok(Some(removed)),
        Err(graphrt_learn::Error::NotFound { .. }) => Ok(Some(0)),
        Err(e) => Err(CliError::failed(format!("the notes were erased, the lessons could not be: {e}"))),
    }
}

#[cfg(not(feature = "learning"))]
fn forget_lessons(_dir: Option<PathBuf>, _agent: &str, _user: &str, _actor: &str) -> Result<Option<usize>, CliError> {
    Ok(None)
}
