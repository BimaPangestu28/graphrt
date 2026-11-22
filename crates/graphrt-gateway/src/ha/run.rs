//! `graphrt gateway` with an `ha:` section: wait as a passive, take over,
//! serve as the fenced active, step down when superseded.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use super::fence::{Clock, Fence, monotonic_clock};
use super::lease::{Beat, Candidate, FileLease, Holder, Promotion};
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
    let heartbeat = spawn_heartbeat(medium, holder, fence.clone(), &ha, clock);
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
        axum::serve(listener, service).with_graceful_shutdown(async move { graceful.notified().await }).await
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

/// The heartbeat thread: a plain thread so a stalled reactor or a slow
/// volume never delays the other.
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

fn spawn_heartbeat(
    mut medium: FileLease,
    mut holder: Holder,
    fence: Arc<Fence>,
    ha: &HaConfig,
    clock: Clock,
) -> Heartbeat {
    let (stop, stopped) = mpsc::channel::<()>();
    let period = ha.heartbeat();
    let fail_after = ha.stale_after_ms;
    let thread = std::thread::spawn(move || {
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
            match holder.beat(&mut medium) {
                Beat::Confirmed => {
                    fence.confirm();
                    unconfirmed_since = None;
                }
                Beat::Unconfirmed(error) => {
                    let now = clock();
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
    });
    Heartbeat { stop, thread }
}
