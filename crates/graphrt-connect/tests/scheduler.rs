//! The scheduler: due sources sync in the background, "sync now", and two
//! schedulers sharing one SQLite state file (the desktop app and
//! `graphrt serve`) never sync the same source at once.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Mock, Reply, build, documents, knowledge};
use graphrt_connect::{
    ConnectError, Scheduler, SourceConfig, SqliteSyncState, SyncOptions, SyncOutcome, SyncSchedule, SyncState, unix_now,
};
use serde_json::json;

fn site_source(mock: &Mock, schedule: SyncSchedule) -> SourceConfig {
    SourceConfig::new("site", "web", json!({ "urls": [format!("{}/", mock.base)], "allow_private": true }))
        .dataset("kb")
        .schedule(schedule)
}

async fn wait_for(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn due_sources_sync_in_the_background_and_manual_ones_wait() {
    let mock = Mock::start().await;
    mock.route("GET /", Reply::html("<title>Home</title><p>Open daily.</p>"));
    let (kb, sink) = knowledge();
    let state: Arc<dyn SyncState> = Arc::new(SqliteSyncState::in_memory().unwrap());
    let scheduler = Scheduler::new(Arc::new(sink), Arc::clone(&state), SyncOptions::default());
    let every = site_source(&mock, SyncSchedule::every_hours(6));
    let manual = SourceConfig { id: "manual".into(), schedule: SyncSchedule::Manual, ..every.clone() };
    scheduler.add(every.clone(), Ok(build(&every, &|_| None)));
    scheduler.add(manual.clone(), Ok(build(&manual, &|_| None)));

    let now = unix_now();
    assert_eq!(scheduler.tick(now), vec!["site".to_owned()], "never synced: due at once; manual is not");
    assert!(wait_for(|| scheduler.source("site").is_some_and(|v| v.status.outcome == SyncOutcome::Succeeded)).await);
    assert_eq!(documents(&kb).await.len(), 1);
    let view = scheduler.source("site").unwrap();
    assert!(!view.syncing);
    let next = view.next_sync_at.unwrap();
    assert!(next >= now + 6 * 3600 - 5, "next sync six hours later");
    assert!(scheduler.tick(now + 60).is_empty(), "not due again yet");
    assert_eq!(scheduler.source("manual").unwrap().next_sync_at, None);

    // "Sync now" works for a manual source.
    let status = scheduler.sync_now("manual").await.unwrap();
    assert_eq!(status.outcome, SyncOutcome::Succeeded);
    assert!(matches!(scheduler.sync_now("nope").await, Err(ConnectError::NotFound(_))));
}

#[tokio::test]
async fn two_processes_sharing_a_state_file_do_not_sync_the_same_source_twice() {
    let mock = Mock::start().await;
    // A slow page keeps the first sync running while the second one tries.
    mock.route("GET /", Reply::html("<title>Home</title><p>Slow page.</p>"));
    let dir = std::env::temp_dir().join(format!("graphrt-connect-sched-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("kb.sqlite.sync");
    let desktop_state: Arc<dyn SyncState> = Arc::new(SqliteSyncState::open(&path).unwrap());
    let serve_state: Arc<dyn SyncState> = Arc::new(SqliteSyncState::open(&path).unwrap());
    let (_kb, sink) = knowledge();
    let sink = Arc::new(sink);
    let desktop = Scheduler::new(
        sink.clone(),
        desktop_state.clone(),
        SyncOptions { holder: "desktop".into(), ..SyncOptions::default() },
    );
    let serve = Scheduler::new(sink, serve_state, SyncOptions { holder: "serve".into(), ..SyncOptions::default() });
    let source = site_source(&mock, SyncSchedule::every_hours(1));
    desktop.add(source.clone(), Ok(build(&source, &|_| None)));
    serve.add(source.clone(), Ok(build(&source, &|_| None)));

    // The desktop app holds the lock (as during a long sync).
    let now = unix_now();
    assert!(desktop_state.try_lock("site", "desktop", now, now + 600).unwrap());
    assert!(matches!(serve.sync_now("site").await, Err(ConnectError::Busy(_))), "serve waits for the desktop sync");
    desktop_state.unlock("site", "desktop").unwrap();

    // The desktop sync finishes; serve sees the shared status and is not due.
    let status = desktop.sync_now("site").await.unwrap();
    assert_eq!(status.outcome, SyncOutcome::Succeeded);
    assert!(serve.tick(unix_now()).is_empty(), "already synced by the other process");
    assert_eq!(serve.source("site").unwrap().status.report.indexed, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_source_that_cannot_be_built_shows_why() {
    let (_kb, sink) = knowledge();
    let scheduler =
        Scheduler::new(Arc::new(sink), Arc::new(graphrt_connect::MemorySyncState::new()), SyncOptions::default());
    let source = SourceConfig::new("drive", "google_drive", json!({ "folder_id": "root" }))
        .schedule(SyncSchedule::every_hours(1));
    scheduler.add(source, Err(ConnectError::Auth("the sign-in (refresh token) is not set".into())));
    assert!(scheduler.tick(unix_now()).is_empty(), "never started");
    let view = scheduler.source("drive").unwrap();
    assert!(view.config_error.as_deref().is_some_and(|e| e.contains("refresh token")));
    assert!(matches!(scheduler.sync_now("drive").await, Err(ConnectError::Auth(_))));
}
