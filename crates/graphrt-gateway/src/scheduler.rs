//! Cron runner: fires [`Schedule`]s of a deployment, with catch-up after a
//! downtime, deterministic jitter and prewarm (see [`crate::cron`]).
//!
//! The last fire time of each schedule is kept at
//! `d/<deployment>/cron/<schedule>/last` in the coordination store; the run
//! record of a fire (`run_id = H(deployment, schedule, fire_time)`) is the
//! idempotency marker, so a fire repeated after a crash is a duplicate, never
//! a second run (I3). Skipped fires are logged with their reason.

use std::sync::Arc;
use std::time::Duration;

use graphrt_core::coordination::{CoordinationStore, deployment_key};
use graphrt_core::store::now_ms;

use crate::cron::{Schedule, fire_run_id};
use crate::deployment::Deployment;
use crate::metrics::bump;
use crate::supervisor::{Gateway, RunInput, StartOptions, Started};

/// A schedule with the run input it fires.
#[derive(Debug, Clone)]
pub struct CronJob {
    /// The schedule.
    pub schedule: Schedule,
    /// Run request body (`inputs`, `metadata`...).
    pub body: serde_json::Map<String, serde_json::Value>,
}

fn last_key(deployment: &str, schedule: &str) -> String {
    format!("{}/last", deployment_key(deployment, "cron", schedule))
}

async fn load_last(gateway: &Gateway, key: &str) -> Option<(i64, u64)> {
    let object = gateway.store.get_object(key).await.ok()??;
    let text = String::from_utf8(object.value).ok()?;
    Some((text.trim().parse().ok()?, object.version))
}

async fn store_last(gateway: &Gateway, key: &str, at: i64, version: Option<u64>) -> Option<u64> {
    let value = at.to_string().into_bytes();
    let result = match version {
        Some(version) => gateway.store.compare_and_swap(key, version, value).await,
        None => gateway.store.create_if_absent(key, value).await,
    };
    result.map_err(|error| eprintln!("gateway: cron: storing last fire of {key}: {error}")).ok()
}

/// Starts the run of one fire. Returns whether a new run was created.
pub async fn fire(gateway: &Arc<Gateway>, deployment: &Arc<Deployment>, job: &CronJob, at: i64) -> bool {
    let run_id = fire_run_id(&deployment.name, &job.schedule.id, at);
    let options = StartOptions {
        run_id: Some(run_id),
        interactive: false,
        callback: None,
        metadata: vec![("cron.schedule".into(), job.schedule.id.clone()), ("cron.fire_time".into(), at.to_string())],
    };
    match gateway.start_run(deployment, RunInput { body: job.body.clone(), expires_at: None }, options).await {
        Ok(Started::New(_)) => {
            bump(&gateway.metrics.cron_fires);
            true
        }
        Ok(Started::Duplicate(_)) => false,
        Err(error) => {
            eprintln!("gateway: cron {}/{} at {at}: {error}", deployment.name, job.schedule.id);
            false
        }
    }
}

fn log_skip(gateway: &Gateway, deployment: &str, schedule: &str, at: i64, reason: &str) {
    bump(&gateway.metrics.cron_skipped);
    eprintln!(
        "{{\"audit\":\"cron_skipped\",\"deployment\":\"{deployment}\",\"schedule\":\"{schedule}\",\"fire_time\":{at},\"reason\":\"{reason}\"}}"
    );
}

/// Runs one job forever.
pub async fn run_job(gateway: Arc<Gateway>, deployment: Arc<Deployment>, job: CronJob) {
    let key = last_key(&deployment.name, &job.schedule.id);
    let now = (now_ms() / 1000) as i64;
    let (mut last, mut version) = match load_last(&gateway, &key).await {
        Some((last, version)) => (last, Some(version)),
        None => (now, None),
    };
    let plan = job.schedule.plan_catchup(last, now);
    for skipped in &plan.skipped {
        log_skip(&gateway, &deployment.name, &job.schedule.id, skipped.at, skipped.reason);
    }
    for at in &plan.run {
        fire(&gateway, &deployment, &job, *at).await;
    }
    last = plan.run.last().copied().unwrap_or(now).max(last);
    version = store_last(&gateway, &key, last, version).await.or(version);
    loop {
        let mut gaps = Vec::new();
        let Some((next, _)) = job.schedule.next_after(last, &mut gaps) else {
            eprintln!("gateway: cron {}/{} never fires again", deployment.name, job.schedule.id);
            return;
        };
        for gap in gaps {
            log_skip(&gateway, &deployment.name, &job.schedule.id, gap.at, gap.reason);
        }
        let due = next.at + job.schedule.jitter(&deployment.name, next.at) as i64;
        let prewarm_at = due - job.schedule.prewarm_secs as i64;
        sleep_until_unix(prewarm_at).await;
        if job.schedule.prewarm_secs > 0
            && let Ok(current) = deployment.current()
        {
            // Jittered (below half the prewarm window) so fires at the same minute do not wake together.
            let window = Duration::from_secs(job.schedule.prewarm_secs) / 2;
            let request = crate::pool::PrewarmRequest::one(crate::warm::PrewarmSource::Cron, window);
            current.pool.spawn_prewarm(request);
        }
        sleep_until_unix(due).await;
        fire(&gateway, &deployment, &job, next.at).await;
        last = next.at;
        version = store_last(&gateway, &key, last, version).await.or(version);
    }
}

async fn sleep_until_unix(at: i64) {
    loop {
        let now_ms = now_ms() as i64;
        let wait = at * 1000 - now_ms;
        if wait <= 0 {
            return;
        }
        // Re-check the wall clock at least every minute (clock jumps, suspend).
        tokio::time::sleep(Duration::from_millis(wait.min(60_000) as u64)).await;
    }
}
