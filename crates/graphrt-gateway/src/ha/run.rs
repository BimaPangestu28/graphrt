//! `graphrt gateway` with an `ha:` section: wait as a passive, take over,
//! serve as the fenced active, step down when superseded.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use super::fence::{Clock, Fence, monotonic_clock};
use super::lease::{Beat, Candidate, FileLease, Holder, LeaseMedium, Mark, MarkFile, Promotion};
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
/// The beat thread does the durable part: the lease's heartbeat counter
/// (lock, write, fsync, rename), and steps down when the lease names another
/// token. The marker thread keeps the active alive in the eyes of the
/// passives: every beat it reads the lease, and while it names us it
/// overwrites the liveness mark in place and confirms the fence. A lease
/// write stuck for seconds (a busy volume, a virus scanner) then only delays
/// the counter: the mark keeps moving, no passive takes the active for
/// frozen, and the beat thread reports the slow write once it returns.
struct Heartbeat {
    stop: mpsc::Sender<()>,
    thread: std::thread::JoinHandle<bool>,
    marking: Arc<AtomicBool>,
    marker: Option<std::thread::JoinHandle<bool>>,
}

impl Heartbeat {
    /// Stops beating and releases the lease if it is still ours.
    fn stop(self) -> bool {
        self.marking.store(false, Ordering::Relaxed);
        if let Some(marker) = self.marker {
            let _joined = marker.join();
        }
        let _sent = self.stop.send(());
        self.thread.join().unwrap_or(false)
    }
}

/// The body of a heartbeat thread. Both threads are spawned as this one
/// type, so the binary carries `std::thread::spawn` once for them.
type ThreadBody = Box<dyn FnOnce() -> bool + Send>;

fn spawn_thread(body: ThreadBody) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(body)
}

fn spawn_heartbeat<M: LeaseMedium + Send + 'static>(
    mut medium: M,
    lease_dir: PathBuf,
    mut holder: Holder,
    fence: Arc<Fence>,
    ha: &HaConfig,
    clock: Clock,
) -> Heartbeat {
    let (stop, stopped) = mpsc::channel::<()>();
    let period = ha.heartbeat();
    let fail_after = ha.stale_after_ms;
    let marking = Arc::new(AtomicBool::new(true));
    let marker = spawn_marker(&lease_dir, &holder, &fence, &marking, period);
    let thread = spawn_thread(Box::new(move || {
        let mut unconfirmed_since: Option<u64> = None;
        loop {
            match stopped.recv_timeout(period) {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if fence.is_tripped() {
                        return false;
                    }
                    return holder.release(&mut medium).unwrap_or(false);
                }
            }
            let started = clock();
            let beat = holder.beat(&mut medium);
            let now = clock();
            let took = now.saturating_sub(started);
            if took > fail_after {
                eprintln!("gateway: ha: lease write took {took} ms");
            }
            match beat {
                Beat::Confirmed => {
                    fence.confirm();
                    unconfirmed_since = None;
                }
                Beat::Unconfirmed(error) => {
                    let since = *unconfirmed_since.get_or_insert(now);
                    if now == since {
                        eprintln!("gateway: ha: heartbeat not confirmed: {error}");
                    }
                    if now.saturating_sub(since) > fail_after {
                        eprintln!("gateway: ha: lease unconfirmed for {} ms; stepping down", now - since);
                        fence.trip();
                        return false;
                    }
                }
                Beat::Lost(by) => {
                    let by = by.map_or_else(
                        || "nobody".to_owned(),
                        |record| format!("{} (token {})", record.node, record.token),
                    );
                    eprintln!("{{\"audit\":\"ha_superseded\",\"token\":{},\"by\":\"{by}\"}}", holder.token());
                    fence.trip();
                    return false;
                }
            }
        }
    }));
    Heartbeat { stop, thread, marking, marker }
}

/// The marker thread (see [`Heartbeat`]). `None` when the mark file cannot
/// be opened: liveness then rests on the counter alone, as without marks.
fn spawn_marker(
    lease_dir: &Path,
    holder: &Holder,
    fence: &Arc<Fence>,
    marking: &Arc<AtomicBool>,
    period: Duration,
) -> Option<std::thread::JoinHandle<bool>> {
    let mut mark = MarkFile::open(lease_dir).map_err(|error| eprintln!("gateway: ha: liveness mark: {error}")).ok()?;
    // Token and pid name the holder: a token is never taken twice.
    let (dir, pid, token) = (lease_dir.to_path_buf(), holder.identity().pid, holder.token());
    let (fence, marking) = (fence.clone(), marking.clone());
    Some(spawn_thread(Box::new(move || {
        let mut seq = 0;
        while marking.load(Ordering::Relaxed) && !fence.is_tripped() {
            std::thread::sleep(period);
            // Only while the lease names us: a lost lease is the beat thread's to act on.
            let ours =
                FileLease::peek(&dir).ok().flatten().is_some_and(|record| record.token == token && record.pid == pid);
            seq += 1;
            if ours && mark.write(Mark { token, seq }).is_ok() {
                fence.confirm();
            }
        }
        true
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    use super::*;
    use crate::ha::lease::{Identity, LeaseRecord, LeaseSettings, Takeover};

    /// No write took the stall yet.
    const NOT_STALLED: u64 = u64::MAX;

    /// A lease volume whose next write stalls for `stall_ms` once it is set:
    /// the heartbeat's write then sits inside the lease lock, as it did on
    /// Windows CI for about three seconds.
    struct SlowVolume {
        inner: FileLease,
        stall_ms: Arc<AtomicU64>,
        /// The heartbeat counter of the write that took the stall.
        stalled_seq: Arc<AtomicU64>,
        /// Every write: counter, ms since the test began, ms it took.
        writes: Arc<Mutex<Vec<(u64, u128, u128)>>>,
        began: Instant,
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
            let started = Instant::now();
            let stall = self.stall_ms.swap(0, Ordering::SeqCst);
            if stall > 0 {
                self.stalled_seq.store(record.seq, Ordering::SeqCst);
            }
            std::thread::sleep(Duration::from_millis(stall));
            let written = self.inner.write(record);
            let entry = (record.seq, (started - self.began).as_millis(), started.elapsed().as_millis());
            self.writes.lock().unwrap().push(entry);
            written
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

        let began = Instant::now();
        let mut volume = SlowVolume {
            inner: FileLease::open(&dir, "a").unwrap(),
            stall_ms: Arc::default(),
            stalled_seq: Arc::new(AtomicU64::new(NOT_STALLED)),
            writes: Arc::default(),
            began,
        };
        let promotion = Candidate::new(identity("a", 1), settings).step(&mut volume, clock()).unwrap().unwrap();
        let holder = Holder::new(identity("a", 1), &promotion);
        let fence = Arc::new(Fence::new(promotion.token, ha.self_fence_window_ms(), clock.clone()));
        fence.confirm();
        let (stall, stalled_seq, writes) =
            (Arc::clone(&volume.stall_ms), Arc::clone(&volume.stalled_seq), Arc::clone(&volume.writes));
        let heartbeat = spawn_heartbeat(volume, dir.clone(), holder, Arc::clone(&fence), &ha, clock.clone());

        // A passive on the same host polls while the active's next lease write stalls for 3 s,
        // five times `stale_after`. It watches until a write after the stalled one landed: the
        // volume's own writes can take seconds more on a busy runner (Windows), so no fixed
        // window says when that is. The deadline only turns a heartbeat that never comes back
        // into a failure instead of a hang.
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        stall.store(3_000, Ordering::SeqCst);
        let mut medium = FileLease::open(&dir, "b").unwrap();
        let mut passive = Candidate::new(identity("b", 2), settings);
        let deadline = Duration::from_secs(60);
        let mut lock_was_busy = false;
        loop {
            lock_was_busy |= !medium.try_lock_mutex().unwrap();
            medium.unlock_mutex();
            let promotion = passive.step(&mut medium, clock()).unwrap();
            assert_eq!(promotion, None, "the passive took a live active at {:?}", started.elapsed());
            assert!(fence.check_local().is_ok(), "the active fenced itself off at {:?}", started.elapsed());
            let stalled = stalled_seq.load(Ordering::SeqCst);
            let landed = FileLease::peek(&dir).ok().flatten().map(|record| record.seq);
            if stalled != NOT_STALLED && landed.is_some_and(|seq| seq > stalled) {
                break;
            }
            assert!(
                started.elapsed() < deadline,
                "no lease write landed after the stalled one (seq {stalled}) in {deadline:?}; \
                 writes (seq, at ms, took ms): {:?}",
                writes.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(3_000), "the passive watched for the whole stall, not {waited:?}");
        assert!(lock_was_busy, "the stall held the lease lock, as a slow write does");
        assert!(!fence.is_tripped());
        eprintln!("writes (seq, at ms, took ms): {:?}", writes.lock().unwrap());

        // A graceful stop still releases the lease: the passive takes it at once.
        assert!(heartbeat.stop());
        let taken = passive.step(&mut medium, clock()).unwrap().unwrap();
        assert_eq!(taken.reason, crate::ha::lease::TakeReason::Released);
        drop(medium);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
