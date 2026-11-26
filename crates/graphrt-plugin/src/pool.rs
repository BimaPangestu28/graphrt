//! A warm pool of plugin processes: `size` processes of one process plugin,
//! started ahead of the first request and replaced in the background when they
//! exit, so a request never waits for the plugin's start (a Node.js host takes
//! ~50 ms) unless the pool is cold.
//!
//! - **Members.** Each member is a [`ProcessTransport`] (one process, its own
//!   restart backoff and timeout tracking). A request goes to a started member
//!   with the fewest requests in flight; the request id names that member, so a
//!   timeout is blamed on the process that served it.
//! - **Keeper.** One background task per pool starts every member, waits until
//!   one exits (crash, wedged process killed after timeouts), and starts it again
//!   at once. A process that dies within [`SHORT_LIFE`] of its start counts as a
//!   failure and the keeper backs off (200 ms doubling to 5 s); after
//!   [`MAX_KEEPER_FAILURES`] in a row it stops and requests start the plugin
//!   lazily, as without a pool, until the next warm hint.
//! - **Idle.** With an idle timeout, the keeper stops every process once the pool
//!   served no request for that long (counted from the last request, warm
//!   hint or finished start), freeing their memory. The next request (or
//!   warm hint, e.g. from a gateway prewarm) fills the pool again.
//! - **Isolation.** Members share nothing. The plugin protocol keeps no state in
//!   a process between requests (a paused node's state travels with the host's
//!   resume request), so any member may serve any request, including a resume.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use graphrt_core::node::Clock;

use crate::conn::{Connection, NotifySink};
use crate::error::CallError;
use crate::manifest::{LoadedManifest, ProcessRuntime};
use crate::process::{ProcessTransport, restart_backoff};
use crate::transport::Transport;

/// Largest pool size: a bound on the memory one plugin can take.
pub const MAX_POOL_SIZE: usize = 32;

/// A process that exits sooner than this after its start counts as a failure.
const SHORT_LIFE: Duration = Duration::from_secs(1);

/// Failed fills or short-lived processes in a row after which the keeper stops.
const MAX_KEEPER_FAILURES: u64 = 6;

/// How a host keeps process plugins warm (see [`crate::PluginHost::with_pool`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolConfig {
    /// Processes kept started per process plugin. `0` (the default) keeps the
    /// plain behaviour: one process, started at the first request.
    pub size: usize,
    /// Stop a pool's processes after this long without a request (`None`: never).
    pub idle_timeout: Option<Duration>,
}

impl PoolConfig {
    /// Whether process plugins get a pool.
    pub fn enabled(&self) -> bool {
        self.size > 0
    }
}

struct Member {
    transport: ProcessTransport,
    in_flight: AtomicUsize,
}

/// What the keeper shares with the pool without keeping it alive.
#[derive(Default)]
struct Signals {
    /// Milliseconds since `epoch` of the last request start or end.
    last_used_ms: AtomicU64,
    /// Requests in flight over every member.
    in_flight: AtomicUsize,
    /// A request or warm hint asks for a full pool.
    wake: tokio::sync::Notify,
    /// The pool was dropped.
    stop: tokio::sync::Notify,
}

struct Inner {
    name: String,
    members: Vec<Member>,
    idle_timeout: Option<Duration>,
    epoch: Instant,
    signals: Arc<Signals>,
    keeper_started: AtomicBool,
    next_id: AtomicU64,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.signals.stop.notify_one();
    }
}

/// A [`Transport`] over a warm pool of processes of one plugin (see the
/// [module](self) docs).
pub struct ProcessPool {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ProcessPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessPool")
            .field("plugin", &self.inner.name)
            .field("size", &self.inner.members.len())
            .field("idle_timeout", &self.inner.idle_timeout)
            .finish_non_exhaustive()
    }
}

impl ProcessPool {
    /// A pool of `config.size` processes (at least one, at most
    /// [`MAX_POOL_SIZE`]) of `plugin`. Nothing starts before
    /// [`Transport::initialize`] or the first request.
    pub fn new(plugin: &LoadedManifest, runtime: &ProcessRuntime, config: PoolConfig) -> Self {
        let size = config.size.clamp(1, MAX_POOL_SIZE);
        let members = (0..size)
            .map(|_| Member {
                transport: ProcessTransport::new(plugin.clone(), runtime.clone()),
                in_flight: AtomicUsize::new(0),
            })
            .collect();
        Self {
            inner: Arc::new(Inner {
                name: plugin.manifest.name.clone(),
                members,
                idle_timeout: config.idle_timeout.filter(|idle| !idle.is_zero()),
                epoch: Instant::now(),
                signals: Arc::default(),
                keeper_started: AtomicBool::new(false),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// Number of processes up right now.
    pub fn ready(&self) -> usize {
        self.inner.members.iter().filter(|m| m.transport.is_ready()).count()
    }

    /// Ask for a full pool now (starts the keeper on first use). A warm hint
    /// counts as use: the pool stays up for its idle timeout after it. Needs a
    /// Tokio runtime.
    fn warm(&self) {
        self.touch();
        self.start_keeper();
        self.inner.signals.wake.notify_one();
    }

    fn start_keeper(&self) {
        if !self.inner.keeper_started.load(Ordering::Relaxed) && !self.inner.keeper_started.swap(true, Ordering::SeqCst)
        {
            graphrt_core::task::spawn(keep(Arc::downgrade(&self.inner), Arc::clone(&self.inner.signals)));
        }
    }

    /// The member a new request goes to (index and member): a started one
    /// with the fewest requests in flight, else the least busy.
    fn pick(&self) -> Option<(usize, &Member)> {
        let members = self.inner.members.iter().enumerate();
        members.min_by_key(|(_, m)| (!m.transport.is_ready(), m.in_flight.load(Ordering::Relaxed)))
    }

    /// The member named by request `id` (`p<member>-<n>`).
    fn member_of(&self, id: &str) -> Option<&Member> {
        let index = id.strip_prefix('p')?.split_once('-')?.0.parse::<usize>().ok()?;
        self.inner.members.get(index)
    }

    fn touch(&self) {
        touch(&self.inner.signals, self.inner.epoch);
    }
}

/// Records a use now (a request, a warm hint, a finished fill).
fn touch(signals: &Signals, epoch: Instant) {
    let now = u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
    signals.last_used_ms.store(now, Ordering::Relaxed);
}

/// Counts a request in flight until dropped (also when the caller abandons it).
struct InFlight<'a> {
    member: &'a Member,
    signals: &'a Signals,
}

impl<'a> InFlight<'a> {
    fn new(member: &'a Member, signals: &'a Signals) -> Self {
        member.in_flight.fetch_add(1, Ordering::SeqCst);
        signals.in_flight.fetch_add(1, Ordering::SeqCst);
        Self { member, signals }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.member.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.signals.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Transport for ProcessPool {
    fn next_id(&self) -> String {
        let member = self.pick().map_or(0, |(index, _)| index);
        format!("p{member}-{}", self.inner.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn call<'a>(
        &'a self,
        id: String,
        method: &'a str,
        params: serde_json::Value,
        sink: Option<NotifySink>,
        cancel_on_drop: bool,
        clock: &'a Arc<dyn Clock>,
    ) -> BoxFuture<'a, Result<serde_json::Value, CallError>> {
        Box::pin(async move {
            let Some(member) = self.member_of(&id).or_else(|| self.pick().map(|(_, member)| member)) else {
                return Err(no_members());
            };
            self.touch();
            // A pool short of processes (idle-stopped, never warmed, one
            // starting again) refills in the background while this request
            // waits for its own member only.
            if self.ready() < self.inner.members.len() {
                self.warm();
            } else {
                self.start_keeper();
            }
            let in_flight = InFlight::new(member, &self.inner.signals);
            let result = member.transport.call(id, method, params, sink, cancel_on_drop, clock).await;
            drop(in_flight);
            self.touch();
            result
        })
    }

    fn initialize<'a>(&'a self, clock: &'a Arc<dyn Clock>) -> BoxFuture<'a, Result<serde_json::Value, CallError>> {
        Box::pin(async move {
            self.warm();
            // The handshake of one member; the keeper starts the others.
            let (_, member) = self.pick().ok_or_else(no_members)?;
            member.transport.initialize(clock).await
        })
    }

    fn ready_for<'a>(&'a self, id: &'a str, clock: &'a Arc<dyn Clock>) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            // The member the request goes to (its id names it); the others
            // refill in the background as for `call`.
            let Some(member) = self.member_of(id).or_else(|| self.pick().map(|(_, member)| member)) else {
                return Err(no_members());
            };
            if self.ready() < self.inner.members.len() {
                self.warm();
            }
            member.transport.ready_for(id, clock).await
        })
    }

    fn note_timeout_of(&self, id: &str) {
        if let Some(member) = self.member_of(id) {
            member.transport.note_timeout();
        }
    }

    fn note_answer_of(&self, id: &str) {
        if let Some(member) = self.member_of(id) {
            member.transport.note_answer();
        }
    }

    fn is_warm(&self) -> bool {
        self.ready() > 0
    }
}

fn no_members() -> CallError {
    CallError::Start("the plugin pool has no processes".into())
}

/// Why the keeper woke up.
enum Event {
    /// A started process exited.
    Exited,
    /// The pool served nothing for its idle timeout.
    Idle,
    /// A request or warm hint asked for a full pool.
    Wake,
    /// The pool was dropped.
    Stop,
}

/// The keeper task of one pool (see the [module](self) docs).
async fn keep(pool: Weak<Inner>, signals: Arc<Signals>) {
    let clock = graphrt_core::node::default_clock();
    let mut failures = 0u64;
    loop {
        let Some(inner) = pool.upgrade() else { return };
        let fills = inner.members.iter().map(|member| member.transport.started(&clock));
        let started: Vec<Arc<Connection>> =
            futures::future::join_all(fills).await.into_iter().filter_map(Result::ok).collect();
        let complete = started.len() == inner.members.len();
        let idle_timeout = inner.idle_timeout;
        let epoch = inner.epoch;
        drop(inner);
        // The idle clock starts once the processes are up: a start slower than
        // the idle timeout must not stop them before anything could use them.
        touch(&signals, epoch);
        if !complete {
            failures += 1;
        }
        if failures >= MAX_KEEPER_FAILURES {
            forward_give_up(&pool);
            drop(started);
            if let Event::Stop = wait_for_wake(&signals).await {
                return;
            }
            failures = 0;
            continue;
        }
        let filled_at = Instant::now();
        // An incomplete fill is retried at once: each member backs off on its own.
        let event = if complete { wait_for_event(&signals, &started, idle_timeout, epoch).await } else { Event::Wake };
        match event {
            Event::Stop => return,
            Event::Exited if filled_at.elapsed() < SHORT_LIFE => {
                failures += 1;
                if let Some(backoff) = restart_backoff(failures) {
                    tokio::time::sleep(backoff).await;
                }
            }
            Event::Exited | Event::Wake => {
                if complete {
                    failures = 0;
                }
            }
            Event::Idle => {
                stop_members(&pool, "plugin pool idle").await;
                drop(started);
                if let Event::Stop = wait_for_wake(&signals).await {
                    return;
                }
                failures = 0;
            }
        }
    }
}

/// Wait until a started process exits, the pool goes idle, a warm hint comes
/// or the pool is dropped.
async fn wait_for_event(
    signals: &Signals,
    started: &[Arc<Connection>],
    idle_timeout: Option<Duration>,
    epoch: Instant,
) -> Event {
    let exits = futures::future::select_all(started.iter().map(|conn| Box::pin(conn.closed())));
    tokio::select! {
        _ = exits => Event::Exited,
        () = idle(signals, idle_timeout, epoch) => Event::Idle,
        () = signals.wake.notified() => Event::Wake,
        () = signals.stop.notified() => Event::Stop,
    }
}

/// Wait for a warm hint or the pool's drop.
async fn wait_for_wake(signals: &Signals) -> Event {
    tokio::select! {
        () = signals.wake.notified() => Event::Wake,
        () = signals.stop.notified() => Event::Stop,
    }
}

/// Resolves once no request ran for `idle_timeout` (never without one).
async fn idle(signals: &Signals, idle_timeout: Option<Duration>, epoch: Instant) {
    let Some(idle_timeout) = idle_timeout else {
        return std::future::pending().await;
    };
    loop {
        let last_used = Duration::from_millis(signals.last_used_ms.load(Ordering::Relaxed));
        let quiet = epoch.elapsed().saturating_sub(last_used);
        if signals.in_flight.load(Ordering::SeqCst) == 0 && quiet >= idle_timeout {
            return;
        }
        tokio::time::sleep(idle_timeout.saturating_sub(quiet).max(Duration::from_millis(50))).await;
    }
}

async fn stop_members(pool: &Weak<Inner>, reason: &str) {
    let Some(inner) = pool.upgrade() else { return };
    for member in &inner.members {
        member.transport.stop(reason).await;
    }
}

fn forward_give_up(pool: &Weak<Inner>) {
    if let Some(inner) = pool.upgrade() {
        crate::conn::forward_log(
            &inner.name,
            "warn",
            "plugin pool stopped restarting processes after repeated failures; requests start the plugin on demand",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(size: usize) -> ProcessPool {
        let manifest = crate::PluginManifest::from_json(
            r#"{"name":"p","runtime":"process","command":"/nonexistent/p","nodes":[{"kind":"p.x","title":"X"}]}"#,
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let crate::Runtime::Process(runtime) = manifest.runtime.clone() else { panic!("a process manifest") };
        let plugin = LoadedManifest { manifest, dir: std::env::temp_dir() };
        ProcessPool::new(&plugin, &runtime, PoolConfig { size, idle_timeout: Some(Duration::ZERO) })
    }

    #[test]
    fn the_size_is_bounded_and_zero_means_off() {
        assert!(!PoolConfig::default().enabled());
        assert!(PoolConfig { size: 1, idle_timeout: None }.enabled());
        assert_eq!(pool(0).inner.members.len(), 1, "an enabled pool has one process at least");
        assert_eq!(pool(1000).inner.members.len(), MAX_POOL_SIZE);
        assert_eq!(pool(1).inner.idle_timeout, None, "a zero idle timeout means never");
    }

    #[test]
    fn request_ids_name_their_member() {
        let pool = pool(3);
        let id = pool.next_id();
        assert!(id.starts_with("p0-"), "{id}");
        assert!(pool.member_of("p2-17").is_some());
        assert!(pool.member_of("p3-1").is_none(), "no fourth member");
        assert!(pool.member_of("r-1").is_none());
    }
}
