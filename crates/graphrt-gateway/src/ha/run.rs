//! `graphrt gateway` with an `ha:` section: wait as a passive, take over,
//! serve as the fenced active, step down when superseded.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use super::fence::{Clock, Fence, monotonic_clock};
use super::lease::{Beat, Candidate, FileLease, Holder, LeaseMedium, LeaseRecord, Mark, MarkFile, Promotion};
use super::{HaConfig, identity, kill_holder};
use crate::app::{build_on, open_store, register_versions, shutdown_signal, spawn_background};
use crate::config::GatewayConfig;
use crate::deployment::VersionInfo;
use crate::server::router;
use crate::store::backup::prepare_store;

/// Waits for the lease, then serves until SIGTERM (releasing the lease) or
/// until a newer active supersedes this one (an error: the supervisor
/// restarts the process, which comes back as a passive).
///
/// # Errors
/// Lease volume, store, build and bind failures; being superseded.
pub async fn serve_active_passive(config: GatewayConfig, versions: Vec<(String, VersionInfo)>) -> Result<(), String> {
    let ha = config.ha.clone().ok_or("ha: missing configuration")?;
    let me = identity(ha.node_id.as_deref());
    let lease_dir = ha.lease_dir_for(&config.store);
    let mut medium = FileLease::open(&lease_dir, &me.node)
        .map_err(|error| format!("ha: lease dir {}: {error}", lease_dir.display()))?;
    let clock = monotonic_clock();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    eprintln!(
        "graphrt gateway: passive (node {}, pid {}), waiting for the lease in {}",
        me.node,
        me.pid,
        lease_dir.display()
    );
    let mut candidate = Candidate::new(me.clone(), ha.lease_settings());
    let mut last_error = String::new();
    let promotion = loop {
        match candidate.step(&mut medium, clock()) {
            Ok(Some(promotion)) => break promotion,
            Ok(None) => {}
            Err(error) => {
                let text = error.to_string();
                if text != last_error {
                    eprintln!("gateway: ha: reading the lease: {text}");
                    last_error = text;
                }
            }
        }
        tokio::select! {
            () = &mut shutdown => return Ok(()),
            () = tokio::time::sleep(Duration::from_millis(ha.poll_ms)) => {}
        }
    };
    let promoted_at = std::time::Instant::now();
    if let Some(pid) = promotion.kill {
        let killed = kill_holder(pid).await;
        eprintln!("{{\"audit\":\"ha_killed_stale_holder\",\"pid\":{pid},\"ok\":{killed}}}");
    }
    let holder = Holder::new(me.clone(), &promotion);
    let (fence, holder) = install_fence(&config, &ha, &mut medium, holder, clock.clone()).await?;
    let heartbeat = spawn_heartbeat(medium, lease_dir.clone(), holder, fence.clone(), &ha, clock);
    let outcome = serve_fenced(&config, versions, &ha, &fence, &promotion, promoted_at).await;
    let released = heartbeat.stop();
    match outcome {
        Ok(()) => {
            eprintln!(
                "{{\"audit\":\"ha_released\",\"node\":\"{}\",\"token\":{},\"released\":{released}}}",
                me.node,
                fence.token()
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Verifies / restores the store and raises its fence to our token.
async fn install_fence(
    config: &GatewayConfig,
    ha: &HaConfig,
    medium: &mut FileLease,
    mut holder: Holder,
    clock: Clock,
) -> Result<(Arc<Fence>, Holder), String> {
    let backup_dir = config.backup.as_ref().map(|backup| backup.dir.clone());
    let prepared = prepare_store(&config.store, backup_dir.as_deref(), ha.verify_store)?;
    eprintln!("gateway: ha: store {}: {prepared:?}", config.store.display());
    let fence = Arc::new(Fence::new(holder.token(), ha.self_fence_window_ms(), clock));
    let store = open_store(config)?;
    match store.promote(fence.clone(), |_| None).await {
        Ok(_) => {}
        Err(graphrt_core::store::StoreError::Precondition(_)) => {
            // The store is ahead of the lease file (it was reset): move the lease past it.
            let stored = store.owner_token().await.map_err(|error| error.to_string())?;
            let raised = holder.raise_token(medium, stored + 1).map_err(|error| format!("ha: lease: {error}"))?;
            if !raised {
                return Err("ha: lost the lease while promoting".to_owned());
            }
            fence.set_token(holder.token());
            store.promote(fence.clone(), |_| None).await.map_err(|error| format!("ha: promotion: {error}"))?;
        }
        Err(error) => return Err(format!("ha: promotion: {error}")),
    }
    Ok((fence, holder))
}

async fn serve_fenced(
    config: &GatewayConfig,
    versions: Vec<(String, VersionInfo)>,
    ha: &HaConfig,
    fence: &Arc<Fence>,
    promotion: &Promotion,
    promoted_at: std::time::Instant,
) -> Result<(), String> {
    let store = open_store(config)?.with_fence(fence.clone());
    let built = build_on(config, store).await?;
    register_versions(&built, versions);
    let (queued, running) = built.app.gateway.recover().await.map_err(|error| error.to_string())?;
    spawn_background(&built);
    let listener = bind_with_retry(&config.listen).await?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let previous = promotion.previous.as_ref().map_or_else(String::new, |record| record.node.clone());
    eprintln!(
        "{{\"audit\":\"ha_promoted\",\"token\":{},\"reason\":\"{}\",\"previous\":\"{previous}\",\"tenure\":{},\"ready_ms\":{},\"running\":{running},\"queued\":{queued}}}",
        fence.token(),
        promotion.reason.as_str(),
        promotion.tenure,
        promoted_at.elapsed().as_millis()
    );
    eprintln!("graphrt gateway: active (token {}), listening on http://{address}", fence.token());
    let stop = Arc::new(tokio::sync::Notify::new());
    let service = router(built.app.clone()).into_make_service_with_connect_info::<SocketAddr>();
    let graceful = stop.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(crate::app::public_listener(listener), service)
            .with_graceful_shutdown(async move { graceful.notified().await })
            .await
    });
    tokio::select! {
        () = shutdown_signal() => {}
        () = fence.tripped() => {
            server.abort();
            return Err(format!("ha: superseded by a newer active (this gateway held token {}); stepping down", fence.token()));
        }
        result = &mut server => {
            return match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            };
        }
    }
    stop.notify_one();
    let _finished = tokio::time::timeout(Duration::from_millis(ha.shutdown_grace_ms), &mut server).await;
    server.abort();
    fence.release();
    Ok(())
}

async fn bind_with_retry(listen: &str) -> Result<tokio::net::TcpListener, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::net::TcpListener::bind(listen).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(format!("binding {listen}: {error}")),
        }
    }
}

/// The heartbeat: two plain threads, so a stalled reactor or a slow volume
/// never delays the other.
///
/// The beat thread keeps the active alive in the eyes of the passives: every
/// beat it checks that the lease still names us (a plain read), overwrites
/// the liveness mark in place and confirms the fence. The writer thread does
/// the durable part (the lease's heartbeat counter: lock, write, fsync,
/// rename, then the mark's fsync). A write stuck for seconds (a busy volume,
/// a virus scanner) then only delays the writer: the mark keeps moving, so no
/// passive takes the active for frozen, and the stuck write is reported.
struct Heartbeat {
    stop: mpsc::Sender<()>,
    thread: std::thread::JoinHandle<bool>,
}

impl Heartbeat {
    /// Stops beating and releases the lease if it is still ours.
    fn stop(self) -> bool {
        let _sent = self.stop.send(());
        self.thread.join().unwrap_or(false)
    }
}

/// What the beat thread asks of the writer.
enum Job {
    Beat,
    /// Release the lease (graceful stop).
    Release,
}

fn spawn_heartbeat<M: LeaseMedium + Send + 'static>(
    medium: M,
    lease_dir: PathBuf,
    holder: Holder,
    fence: Arc<Fence>,
    ha: &HaConfig,
    clock: Clock,
) -> Heartbeat {
    let (stop, stopped) = mpsc::channel::<()>();
    let period = ha.heartbeat();
    let fail_after = ha.stale_after_ms;
    let thread = std::thread::spawn(move || {
        let identity = holder.identity().clone();
        let token = holder.token();
        let mut mark = match MarkFile::open(&lease_dir) {
            Ok(mark) => Some(mark),
            Err(error) => {
                // Liveness then rests on the lease counter alone, as without marks.
                eprintln!("gateway: ha: cannot open the liveness mark: {error}");
                None
            }
        };
        let syncer = mark.as_ref().and_then(|mark| mark.try_clone().ok());
        let (jobs, results, writer) = spawn_writer(medium, holder, syncer);
        let mut seq = 0_u64;
        let mut unconfirmed_since: Option<u64> = None;
        // When the writer took the job it has not answered yet.
        let mut writing_since: Option<u64> = None;
        let mut stuck_reported = false;
        let mut write_failing = false;
        let mut outcome = None;
        // Until the stop: a message, or the server side dropped.
        while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(period) {
            // What the writer found since the last beat.
            while let Ok(beat) = results.try_recv() {
                writing_since = None;
                if stuck_reported {
                    eprintln!("gateway: ha: the lease write went through again");
                    stuck_reported = false;
                }
                match beat {
                    Beat::Confirmed => write_failing = false,
                    Beat::Unconfirmed(error) => {
                        if !write_failing {
                            eprintln!("gateway: ha: lease write not confirmed: {error}");
                            write_failing = true;
                        }
                    }
                    Beat::Lost(by) => {
                        report_superseded(token, by.as_ref());
                        outcome = Some(false);
                    }
                }
            }
            if outcome.is_some() {
                break;
            }
            let now = clock();
            let live = match FileLease::peek(&lease_dir) {
                Ok(Some(record)) if record.is_held_by(&identity, token) => {
                    seq += 1;
                    match mark.as_mut().map(|mark| mark.write(Mark { token, seq })) {
                        Some(Err(error)) => Err(format!("liveness mark: {error}")),
                        _ => Ok(()),
                    }
                }
                Ok(Some(record)) => {
                    report_superseded(token, Some(&record));
                    outcome = Some(false);
                    break;
                }
                // Gone or unreadable: the writer decides under the lock.
                Ok(None) => Err("the lease is missing".to_owned()),
                Err(error) => Err(error.to_string()),
            };
            match live {
                Ok(()) => {
                    fence.confirm();
                    unconfirmed_since = None;
                }
                Err(error) => {
                    let since = *unconfirmed_since.get_or_insert(now);
                    if now == since {
                        eprintln!("gateway: ha: heartbeat not confirmed: {error}");
                    }
                    if now.saturating_sub(since) > fail_after {
                        eprintln!("gateway: ha: lease unconfirmed for {} ms; stepping down", now - since);
                        fence.trip();
                        outcome = Some(false);
                        break;
                    }
                }
            }
            match writing_since {
                None => {
                    if jobs.send(Job::Beat).is_err() {
                        // The writer is gone: nothing durable can be written any more.
                        fence.trip();
                        outcome = Some(false);
                        break;
                    }
                    writing_since = Some(now);
                }
                Some(since) if !stuck_reported && now.saturating_sub(since) > fail_after => {
                    eprintln!(
                        "gateway: ha: a lease write is stuck for {} ms (slow volume?); the liveness mark keeps this gateway active",
                        now - since
                    );
                    stuck_reported = true;
                }
                Some(_) => {}
            }
        }
        let released = match outcome {
            Some(released) => {
                drop(jobs);
                released
            }
            None if fence.is_tripped() => {
                drop(jobs);
                false
            }
            None => {
                let sent = jobs.send(Job::Release).is_ok();
                // The writer ends after the release: its last answer is the release's.
                drop(jobs);
                sent && wait_release(&results)
            }
        };
        let _joined = writer.join();
        released
    });
    Heartbeat { stop, thread }
}

/// The writer thread: runs the jobs of the beat thread, answers each beat.
/// Ends when the beat thread drops its sender. A release answers `Confirmed`
/// when the lease was still ours.
fn spawn_writer<M: LeaseMedium + Send + 'static>(
    mut medium: M,
    mut holder: Holder,
    syncer: Option<MarkFile>,
) -> (mpsc::Sender<Job>, mpsc::Receiver<Beat>, std::thread::JoinHandle<()>) {
    let (jobs, inbox) = mpsc::channel::<Job>();
    let (outbox, results) = mpsc::channel::<Beat>();
    let writer = std::thread::spawn(move || {
        while let Ok(job) = inbox.recv() {
            let beat = match job {
                Job::Beat => {
                    let beat = holder.beat(&mut medium);
                    if let Some(syncer) = &syncer {
                        // Best effort: the passives of this host read the mark without it.
                        let _synced = syncer.sync();
                    }
                    beat
                }
                Job::Release => match holder.release(&mut medium) {
                    Ok(true) => Beat::Confirmed,
                    Ok(false) => Beat::Lost(None),
                    Err(error) => Beat::Unconfirmed(error.to_string()),
                },
            };
            if outbox.send(beat).is_err() {
                return;
            }
        }
    });
    (jobs, results, writer)
}

/// The answer to [`Job::Release`], after the answers of the beats before it.
fn wait_release(results: &mpsc::Receiver<Beat>) -> bool {
    let mut last = None;
    while let Ok(beat) = results.recv() {
        last = Some(beat);
    }
    matches!(last, Some(Beat::Confirmed))
}

fn report_superseded(token: u64, by: Option<&LeaseRecord>) {
    let by = by.map_or_else(|| "nobody".to_owned(), |record| format!("{} (token {})", record.node, record.token));
    eprintln!("{{\"audit\":\"ha_superseded\",\"token\":{token},\"by\":\"{by}\"}}");
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    use super::*;
    use crate::ha::lease::{Identity, LeaseSettings, Takeover};

    /// A lease volume whose writes stall while `stall_ms` is set: the
    /// heartbeat's write then sits inside the lease lock, as it did on
    /// Windows CI for about three seconds.
    struct SlowVolume {
        inner: FileLease,
        stall_ms: Arc<AtomicU64>,
    }

    impl LeaseMedium for SlowVolume {
        fn try_lock_mutex(&mut self) -> std::io::Result<bool> {
            self.inner.try_lock_mutex()
        }
        fn unlock_mutex(&mut self) {
            self.inner.unlock_mutex();
        }
        fn read(&mut self) -> std::io::Result<Option<LeaseRecord>> {
            self.inner.read()
        }
        fn write(&mut self, record: &LeaseRecord) -> std::io::Result<()> {
            let stall = self.stall_ms.swap(0, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(stall));
            self.inner.write(record)
        }
        fn try_tenure(&mut self) -> std::io::Result<bool> {
            self.inner.try_tenure()
        }
        fn release_tenure(&mut self) {
            self.inner.release_tenure();
        }
        fn take_promote_request(&mut self, node: &str) -> std::io::Result<bool> {
            self.inner.take_promote_request(node)
        }
        fn unix_ms(&self) -> u64 {
            self.inner.unix_ms()
        }
    }

    fn identity(node: &str, pid: u32) -> Identity {
        Identity { node: node.into(), host: "h1".into(), pid }
    }

    #[test]
    fn an_active_whose_lease_write_stalls_is_not_taken_for_frozen() {
        let dir = std::env::temp_dir().join(format!("graphrt-gw-slow-volume-{}", std::process::id()));
        let _cleanup = std::fs::remove_dir_all(&dir);
        let ha: HaConfig = graphrt_core::yaml::from_str("heartbeat_ms: 100\nstale_after_ms: 600\n", false).unwrap();
        let settings = LeaseSettings { takeover: Takeover::Auto, stale_after_ms: 600, kill_stale_holder: true };
        let clock = monotonic_clock();

        let mut volume = SlowVolume { inner: FileLease::open(&dir, "a").unwrap(), stall_ms: Arc::default() };
        let promotion = Candidate::new(identity("a", 1), settings).step(&mut volume, clock()).unwrap().unwrap();
        let holder = Holder::new(identity("a", 1), &promotion);
        let fence = Arc::new(Fence::new(promotion.token, ha.self_fence_window_ms(), clock.clone()));
        fence.confirm();
        let stall = Arc::clone(&volume.stall_ms);
        let heartbeat = spawn_heartbeat(volume, dir.clone(), holder, Arc::clone(&fence), &ha, clock.clone());

        // A passive on the same host polls while the active's next lease write stalls for 3 s,
        // five times `stale_after`.
        std::thread::sleep(Duration::from_millis(300));
        stall.store(3_000, Ordering::SeqCst);
        let mut medium = FileLease::open(&dir, "b").unwrap();
        let mut passive = Candidate::new(identity("b", 2), settings);
        let started = Instant::now();
        let mut lock_was_busy = false;
        while started.elapsed() < Duration::from_millis(4_000) {
            lock_was_busy |= !medium.try_lock_mutex().unwrap();
            medium.unlock_mutex();
            let promotion = passive.step(&mut medium, clock()).unwrap();
            assert_eq!(promotion, None, "the passive took a live active at {:?}", started.elapsed());
            assert!(fence.check_local().is_ok(), "the active fenced itself off at {:?}", started.elapsed());
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(lock_was_busy, "the stall held the lease lock, as a slow write does");
        assert_eq!(stall.load(Ordering::SeqCst), 0, "the stalled write happened");
        assert!(FileLease::peek(&dir).unwrap().unwrap().seq > 3, "lease writes went on after the stall");
        assert!(!fence.is_tripped());

        // A graceful stop still releases the lease: the passive takes it at once.
        assert!(heartbeat.stop());
        let taken = passive.step(&mut medium, clock()).unwrap().unwrap();
        assert_eq!(taken.reason, crate::ha::lease::TakeReason::Released);
        drop(medium);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
