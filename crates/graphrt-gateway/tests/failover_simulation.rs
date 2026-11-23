//! Deterministic failover chaos of active/passive gateways (plan F3, I8).
//!
//! Every seed builds a world of 2–3 gateway processes sharing one SQLite file
//! and one lease, on a simulated clock (100 ms ticks, 10 simulated minutes).
//! The real components run: the lease state machine ([`Candidate`] /
//! [`Holder`]) over a simulated lease volume, the store [`Fence`], a real
//! [`SqliteStore`] handle per process (separate connections to one file),
//! the cron [`Schedule`] with its deterministic run ids, and webhook run ids.
//! Each process's application is modelled: an active fires cron (catch-up
//! `all`) and answers webhook deliveries that a sender retries until
//! acknowledged (and sometimes redelivers after an acknowledgement).
//!
//! Chaos: `kill -9` of the active (also between taking the lease and fencing
//! the store), freezes (SIGSTOP) of any process with a pending write on
//! resume, freezes *inside* the lease lock (mid heartbeat: the lock stays
//! taken and the beat's write lands late on resume; mid poll of a passive),
//! partitions from the volume, graceful handovers, manual promotion
//! requests, wall-clock skew between nodes, and seeds with the self-fence
//! disabled so only the store fence stands between a resumed active and the
//! store.
//!
//! Checked per seed:
//! - **I8**: committed writes carry non-decreasing tokens and one token
//!   belongs to one process incarnation; a takeover only kills the holder.
//! - **I3**: at most one run per cron fire time and no missed fire (catch-up).
//! - **I4**: at most one new run per webhook key; every delivery acknowledged
//!   and its run stored.
//! - Takeover: a fault of the active with a healthy passive is followed by a
//!   new active within 30 s; on one host (`auto`), the lease of an active
//!   frozen for more than twice `stale_after` (inside the lease lock or not)
//!   has moved to a passive before it resumes.
//!
//! `GRAPHRT_SIM_SEEDS` (default 1000; nightly 10000), `GRAPHRT_SIM_SEED_START`,
//! `GRAPHRT_SIM_LANES` (threads, default 8).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::too_many_lines
)]

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use graphrt_core::coordination::CoordinationStore;
use graphrt_core::store::{RunRecord, RunStore, StoreError};
use graphrt_gateway::cron::{Catchup, Schedule, fire_run_id, utc};
use graphrt_gateway::ha::fence::{Clock, Fence};
use graphrt_gateway::ha::lease::{
    Beat, Candidate, Holder, Identity, LeaseMedium, LeaseRecord, LeaseSettings, Promotion, Takeover,
};
use graphrt_gateway::store::SqliteStore;
use graphrt_gateway::webhook::hook_run_id;

const TICK_MS: u64 = 100;
const HEARTBEAT_MS: u64 = 1_000;
const STALE_AFTER_MS: u64 = 5_000;
const POLL_MS: u64 = 200;
const CHAOS_END_MS: u64 = 420_000;
const END_MS: u64 = 600_000;
const TAKEOVER_BOUND_MS: u64 = 30_000;
const DEPLOYMENT: &str = "shop";
const SCHEDULE: &str = "tick";
const CRON_LAST_KEY: &str = "d/shop/cron/tick/last";

/// SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }
    fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }
    fn range(&mut self, low: u64, high: u64) -> u64 {
        low + self.below(high - low + 1)
    }
}

/// The shared lease volume.
#[derive(Default)]
struct Disk {
    lease: Option<LeaseRecord>,
    mutex: Option<usize>,
    tenure: Option<usize>,
    promote: Option<String>,
}

struct SimMedium<'a> {
    disk: &'a mut Disk,
    node: usize,
    partitioned: bool,
    unix_ms: u64,
}

impl SimMedium<'_> {
    fn reachable(&self) -> io::Result<()> {
        if self.partitioned { Err(io::Error::other("volume unreachable")) } else { Ok(()) }
    }
}

impl LeaseMedium for SimMedium<'_> {
    fn try_lock_mutex(&mut self) -> io::Result<bool> {
        self.reachable()?;
        match self.disk.mutex {
            Some(owner) if owner != self.node => Ok(false),
            _ => {
                self.disk.mutex = Some(self.node);
                Ok(true)
            }
        }
    }
    fn unlock_mutex(&mut self) {
        if self.disk.mutex == Some(self.node) {
            self.disk.mutex = None;
        }
    }
    fn read(&mut self) -> io::Result<Option<LeaseRecord>> {
        self.reachable()?;
        Ok(self.disk.lease.clone())
    }
    fn write(&mut self, record: &LeaseRecord) -> io::Result<()> {
        self.reachable()?;
        self.disk.lease = Some(record.clone());
        Ok(())
    }
    fn try_tenure(&mut self) -> io::Result<bool> {
        self.reachable()?;
        match self.disk.tenure {
            Some(owner) if owner != self.node => Ok(false),
            _ => {
                self.disk.tenure = Some(self.node);
                Ok(true)
            }
        }
    }
    fn release_tenure(&mut self) {
        if self.disk.tenure == Some(self.node) {
            self.disk.tenure = None;
        }
    }
    fn take_promote_request(&mut self, node: &str) -> io::Result<bool> {
        self.reachable()?;
        match self.disk.promote.as_deref() {
            Some(target) if target == "*" || target == node => {
                self.disk.promote = None;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
    fn unix_ms(&self) -> u64 {
        self.unix_ms
    }
}

struct Active {
    holder: Holder,
    fence: Arc<Fence>,
    store: SqliteStore,
    cron_last: Option<(i64, Option<u64>)>,
    unconfirmed_since: Option<u64>,
    next_beat: u64,
}

enum State {
    Down { restart_at: u64 },
    Passive { candidate: Candidate, next_poll: u64 },
    Active(Box<Active>),
}

struct Node {
    index: usize,
    incarnation: u32,
    host: String,
    skew_secs: i64,
    state: State,
    frozen_until: Option<u64>,
    pending_write: bool,
    partitioned_until: Option<u64>,
    /// Frozen while holding the lease lock.
    frozen_in_lock: bool,
    /// The lease the frozen heartbeat writes when the process resumes.
    late_lease: Option<LeaseRecord>,
    /// A single-fault freeze of the active that a passive must end: (start, faults, token).
    displace_by_resume: Option<(u64, u64, u64)>,
}

impl Node {
    fn identity(&self) -> Identity {
        Identity { node: format!("gw-{}", self.index), host: self.host.clone(), pid: self.pid() }
    }
    fn pid(&self) -> u32 {
        (self.index as u32 + 1) * 10_000 + self.incarnation
    }
    fn frozen(&self, now: u64) -> bool {
        self.frozen_until.is_some_and(|until| now < until)
    }
    fn partitioned(&self, now: u64) -> bool {
        self.partitioned_until.is_some_and(|until| now < until)
    }
    fn is_active(&self) -> bool {
        matches!(self.state, State::Active(_))
    }
    fn healthy_passive(&self, now: u64) -> bool {
        matches!(self.state, State::Passive { .. }) && !self.frozen(now) && !self.partitioned(now)
    }
}

#[derive(Default)]
struct Delivery {
    key: String,
    next_attempt: u64,
    acked: bool,
    redeliver: bool,
    new_runs: u32,
}

#[derive(Default)]
struct Report {
    violations: Vec<String>,
    takeovers_ms: Vec<u64>,
    promotions: u32,
    commits: u64,
    fenced_writes: u64,
}

struct World {
    seed: u64,
    rng: Rng,
    now: Arc<AtomicU64>,
    clock: Clock,
    disk: Disk,
    nodes: Vec<Node>,
    path: PathBuf,
    settings: LeaseSettings,
    window_ms: u64,
    schedule: Schedule,
    base_unix: i64,
    commits: Vec<(u64, usize, u32)>,
    fires: BTreeMap<i64, u32>,
    deliveries: Vec<Delivery>,
    outage: Option<(u64, u64)>,
    faults: u64,
    report: Report,
}

fn store_error_is_fence(error: &StoreError) -> bool {
    matches!(error, StoreError::Precondition(message) | StoreError::Io(message) if message.starts_with("fenced"))
}

impl World {
    fn new(seed: u64, dir: &std::path::Path) -> Self {
        let mut rng = Rng(seed ^ 0xF3F3_0000_0000_0001);
        let count = if rng.chance(500) { 2 } else { 3 };
        let same_host = rng.chance(600);
        let now = Arc::new(AtomicU64::new(0));
        let reading = now.clone();
        let clock: Clock = Arc::new(move || reading.load(Ordering::Relaxed));
        let mut nodes = Vec::new();
        for index in 0..count {
            let skew_secs = rng.range(0, 60) as i64 - 30;
            nodes.push(Node {
                index,
                incarnation: 0,
                host: if same_host { "h".into() } else { format!("h{index}") },
                skew_secs,
                state: State::Down { restart_at: rng.range(0, 2_000) },
                frozen_until: None,
                pending_write: false,
                partitioned_until: None,
                frozen_in_lock: false,
                late_lease: None,
                displace_by_resume: None,
            });
        }
        let takeover = if rng.chance(800) { Takeover::Auto } else { Takeover::Manual };
        // Some seeds disable the self-fence: only the store fence protects them.
        let window_ms = if rng.chance(300) { u64::MAX / 4 } else { STALE_AFTER_MS - HEARTBEAT_MS };
        let mut schedule = Schedule::new(SCHEDULE, "* * * * *", "UTC").unwrap();
        schedule.catchup = Catchup::All;
        schedule.max_catchup = 1_000;
        let path = dir.join(format!("seed-{seed}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _cleanup = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        let deliveries = (0..rng.range(10, 40))
            .map(|number| Delivery {
                key: format!("evt-{number}"),
                next_attempt: rng.range(0, CHAOS_END_MS),
                redeliver: rng.chance(300),
                ..Delivery::default()
            })
            .collect();
        Self {
            seed,
            rng,
            now,
            clock,
            disk: Disk::default(),
            nodes,
            path,
            settings: LeaseSettings { takeover, stale_after_ms: STALE_AFTER_MS, kill_stale_holder: true },
            window_ms,
            schedule,
            base_unix: utc(2026, 3, 29, 0, 0),
            commits: Vec::new(),
            fires: BTreeMap::new(),
            deliveries,
            outage: None,
            faults: 0,
            report: Report::default(),
        }
    }

    fn time(&self) -> u64 {
        self.now.load(Ordering::Relaxed)
    }

    fn wall(&self, node: usize) -> i64 {
        self.base_unix + (self.time() / 1000) as i64 + self.nodes[node].skew_secs
    }

    fn violation(&mut self, message: String) {
        self.report.violations.push(format!("seed {}: t={}ms {message}", self.seed, self.time()));
    }

    fn medium(&mut self, node: usize) -> SimMedium<'_> {
        let now = self.time();
        let partitioned = self.nodes[node].partitioned(now);
        let unix_ms = (self.base_unix as u64) * 1000 + now;
        SimMedium { disk: &mut self.disk, node, partitioned, unix_ms }
    }

    fn active_index(&self) -> Option<usize> {
        self.nodes.iter().position(|node| node.is_active() && !node.frozen(self.time()))
    }

    fn token_of(&self, node: usize) -> Option<u64> {
        match &self.nodes[node].state {
            State::Active(active) => Some(active.fence.token()),
            _ => None,
        }
    }

    /// Records a committed write for I8.
    fn committed(&mut self, node: usize) {
        let token = self.token_of(node).unwrap_or(0);
        let incarnation = self.nodes[node].incarnation;
        self.commits.push((token, node, incarnation));
        self.report.commits += 1;
    }

    /// The process dies: the kernel drops its locks.
    fn kill(&mut self, node: usize, restart_in: u64) {
        if self.disk.tenure == Some(node) {
            self.disk.tenure = None;
        }
        if self.disk.mutex == Some(node) {
            self.disk.mutex = None;
        }
        let now = self.time();
        let target = &mut self.nodes[node];
        target.state = State::Down { restart_at: now + restart_in };
        target.frozen_until = None;
        target.pending_write = false;
        target.frozen_in_lock = false;
        target.late_lease = None;
        target.displace_by_resume = None;
    }

    fn same_host(&self) -> bool {
        self.nodes.iter().all(|node| node.host == self.nodes[0].host)
    }

    /// Freezes `target` until `until`; sometimes inside the lease lock.
    fn freeze(&mut self, target: usize, until: u64) {
        let now = self.time();
        let node = &self.nodes[target];
        if matches!(node.state, State::Down { .. }) || node.frozen(now) {
            return;
        }
        let in_lock = self.disk.mutex.is_none() && !node.partitioned(now) && self.rng.chance(500);
        if in_lock {
            let before = self.disk.lease.clone();
            let unix_ms = (self.base_unix as u64) * 1000 + now;
            let World { disk, nodes, .. } = self;
            if let State::Active(active) = &mut nodes[target].state {
                // The beat read the lease and wrote its temporary file; the rename lands on resume.
                let mut medium = SimMedium { disk, node: target, partitioned: false, unix_ms };
                if active.holder.beat(&mut medium) == Beat::Confirmed {
                    nodes[target].late_lease = disk.lease.clone();
                }
                disk.lease = before;
            }
            disk.mutex = Some(target);
        }
        let token = self.token_of(target);
        let healthy = self.nodes.iter().any(|node| node.healthy_passive(now));
        let bounded = self.settings.takeover == Takeover::Auto && self.same_host();
        let node = &mut self.nodes[target];
        node.frozen_until = Some(until);
        node.pending_write = true;
        node.frozen_in_lock = in_lock;
        node.displace_by_resume = token
            .filter(|_| bounded && healthy && until - now > 2 * STALE_AFTER_MS + 2 * POLL_MS)
            .map(|token| (now, self.faults, token));
    }

    /// The process continues after a freeze: a heartbeat stopped inside the
    /// lease lock completes its write and releases the lock.
    fn resume(&mut self, node: usize) {
        self.nodes[node].frozen_until = None;
        if let Some((started, faults, token)) = self.nodes[node].displace_by_resume.take()
            && faults == self.faults
            && self.disk.lease.as_ref().is_some_and(|lease| lease.token == token)
        {
            self.violation(format!(
                "active gw-{node} (token {token}) frozen at {started} ms resumed still holding the lease"
            ));
        }
        if std::mem::take(&mut self.nodes[node].frozen_in_lock) {
            if let Some(late) = self.nodes[node].late_lease.take() {
                self.disk.lease = Some(late);
            }
            if self.disk.mutex == Some(node) {
                self.disk.mutex = None;
            }
        }
    }

    fn start_outage(&mut self) {
        let now = self.time();
        let healthy = self.nodes.iter().any(|node| node.healthy_passive(now));
        if self.outage.is_none() && healthy {
            self.outage = Some((now, self.faults));
        }
    }

    fn inject_chaos(&mut self) {
        let now = self.time();
        if now >= CHAOS_END_MS || !self.rng.chance(4) {
            return;
        }
        self.faults += 1;
        let target = self.rng.below(self.nodes.len() as u64) as usize;
        let active = self.active_index();
        match self.rng.below(7) {
            0 | 1 => {
                if let Some(active) = active {
                    self.start_outage();
                    let restart_in = self.rng.range(500, 10_000);
                    self.kill(active, restart_in);
                }
            }
            2 => {
                let target = if self.rng.chance(700) { active.unwrap_or(target) } else { target };
                if self.nodes[target].is_active() {
                    self.start_outage();
                }
                let until = now + self.rng.range(1_000, 25_000);
                self.freeze(target, until);
            }
            3 => {
                let target = if self.rng.chance(600) { active.unwrap_or(target) } else { target };
                if self.nodes[target].is_active() {
                    self.start_outage();
                }
                let until = now + self.rng.range(1_000, 20_000);
                self.nodes[target].partitioned_until = Some(until);
            }
            4 => {
                if let Some(active) = active {
                    self.start_outage();
                    self.handover(active);
                }
            }
            5 => {
                // Planned switchover to a healthy passive.
                let passives: Vec<usize> =
                    (0..self.nodes.len()).filter(|&node| self.nodes[node].healthy_passive(now)).collect();
                if let Some(&chosen) = passives.get(self.rng.below(passives.len().max(1) as u64) as usize) {
                    if active.is_some() {
                        self.start_outage();
                    }
                    self.disk.promote = Some(format!("gw-{chosen}"));
                }
            }
            _ => {
                // Clock jump of one node's wall clock (NTP step).
                let jump = self.rng.range(0, 40) as i64 - 20;
                self.nodes[target].skew_secs += jump;
            }
        }
    }

    /// Graceful shutdown: stop writing, release the lease, exit, restart.
    fn handover(&mut self, node: usize) {
        let State::Active(active) = std::mem::replace(&mut self.nodes[node].state, State::Down { restart_at: 0 })
        else {
            return;
        };
        let mut active = *active;
        active.fence.release();
        let mut medium = self.medium(node);
        let _released = active.holder.release(&mut medium);
        let restart_in = self.rng.range(200, 3_000);
        self.kill(node, restart_in);
    }

    async fn step_passive(&mut self, node: usize) {
        let now = self.time();
        let partitioned = self.nodes[node].partitioned(now);
        let unix_ms = (self.base_unix as u64) * 1000 + now;
        let World { disk, nodes, .. } = self;
        let State::Passive { candidate, next_poll } = &mut nodes[node].state else { return };
        if now < *next_poll {
            return;
        }
        *next_poll = now + POLL_MS;
        let mut medium = SimMedium { disk, node, partitioned, unix_ms };
        let Ok(Some(promotion)) = candidate.step(&mut medium, now) else { return };
        self.promote(node, promotion).await;
    }

    async fn promote(&mut self, node: usize, promotion: Promotion) {
        if let Some(pid) = promotion.kill {
            let holder = self.disk_holder_before(&promotion);
            match self
                .nodes
                .iter()
                .position(|candidate| candidate.pid() == pid && !matches!(candidate.state, State::Down { .. }))
            {
                Some(victim) if Some(victim) == holder => {
                    let restart_in = self.rng.range(500, 5_000);
                    self.kill(victim, restart_in);
                }
                other => {
                    self.violation(format!("takeover would kill pid {pid} ({other:?}), not the holder ({holder:?})"))
                }
            }
        }
        // Crash between taking the lease and fencing the store.
        if self.time() < CHAOS_END_MS && self.rng.chance(20) {
            let restart_in = self.rng.range(500, 5_000);
            self.kill(node, restart_in);
            return;
        }
        let identity = self.nodes[node].identity();
        let mut holder = Holder::new(identity, &promotion);
        let fence = Arc::new(Fence::new(promotion.token, self.window_ms, self.clock.clone()));
        let store = SqliteStore::open(&self.path).unwrap().with_fence(fence.clone());
        let mut result = store.promote(fence.clone(), |_| None).await;
        if matches!(result, Err(StoreError::Precondition(_))) {
            let stored = store.owner_token().await.unwrap();
            let raised = {
                let mut medium = self.medium(node);
                holder.raise_token(&mut medium, stored + 1).unwrap_or(false)
            };
            if raised {
                fence.set_token(holder.token());
                result = store.promote(fence.clone(), |_| None).await;
            }
        }
        if result.is_err() {
            let restart_in = self.rng.range(200, 2_000);
            self.kill(node, restart_in);
            return;
        }
        self.report.promotions += 1;
        let now = self.time();
        if let Some((started, faults)) = self.outage.take() {
            // Only single-fault outages measure the takeover bound.
            if faults == self.faults {
                self.report.takeovers_ms.push(now - started);
            }
        }
        self.nodes[node].state = State::Active(Box::new(Active {
            holder,
            fence,
            store,
            cron_last: None,
            unconfirmed_since: None,
            next_beat: now + HEARTBEAT_MS,
        }));
    }

    fn disk_holder_before(&self, promotion: &Promotion) -> Option<usize> {
        let previous = promotion.previous.as_ref()?;
        self.nodes.iter().position(|node| node.pid() == previous.pid && !matches!(node.state, State::Down { .. }))
    }

    async fn step_active(&mut self, node: usize) {
        let now = self.time();
        let tripped = matches!(&self.nodes[node].state, State::Active(active) if active.fence.is_tripped());
        if tripped {
            let restart_in = self.rng.range(200, 2_000);
            self.kill(node, restart_in);
            return;
        }
        if std::mem::take(&mut self.nodes[node].pending_write) {
            // The write a frozen process was about to make when it stopped.
            let probe = format!("probe-{}-{}-{now}", node, self.nodes[node].incarnation);
            self.write_run(node, &probe).await;
        }
        self.heartbeat(node, now);
        if !self.nodes[node].is_active() {
            return;
        }
        self.run_cron(node).await;
    }

    fn heartbeat(&mut self, node: usize, now: u64) {
        let partitioned = self.nodes[node].partitioned(now);
        let unix_ms = (self.base_unix as u64) * 1000 + now;
        let World { disk, nodes, .. } = self;
        let State::Active(active) = &mut nodes[node].state else { return };
        if now < active.next_beat {
            return;
        }
        active.next_beat = now + HEARTBEAT_MS;
        let mut medium = SimMedium { disk, node, partitioned, unix_ms };
        let beat = active.holder.beat(&mut medium);
        match beat {
            Beat::Confirmed => {
                active.fence.confirm();
                active.unconfirmed_since = None;
            }
            Beat::Unconfirmed(_) => {
                let since = *active.unconfirmed_since.get_or_insert(now);
                if now - since > STALE_AFTER_MS {
                    active.fence.trip();
                }
            }
            Beat::Lost(_) => active.fence.trip(),
        }
    }

    /// One store write of `node`'s application; returns its outcome.
    async fn write<T>(
        &mut self,
        node: usize,
        write: impl std::future::Future<Output = Result<T, StoreError>>,
    ) -> Option<Result<T, StoreError>> {
        if self.nodes[node].partitioned(self.time()) {
            return None;
        }
        let result = write.await;
        match &result {
            Ok(_) => self.committed(node),
            Err(error) if store_error_is_fence(error) => self.report.fenced_writes += 1,
            Err(_) => {}
        }
        Some(result)
    }

    async fn write_run(&mut self, node: usize, run_id: &str) -> Option<Result<(), StoreError>> {
        let State::Active(active) = &self.nodes[node].state else { return None };
        let store = active.store.clone();
        let record = RunRecord::queued(run_id, "wf", "fp", Some(DEPLOYMENT.into()), self.time());
        self.write(node, async move { store.create(record).await }).await
    }

    async fn run_cron(&mut self, node: usize) {
        let now = self.wall(node);
        let State::Active(active) = &self.nodes[node].state else { return };
        let store = active.store.clone();
        let (mut last, mut version) = match active.cron_last {
            Some(state) => state,
            None => {
                // Promotion: load the stored last fire and catch up (real scheduler logic).
                let Ok(stored) = store.get_object(CRON_LAST_KEY).await else { return };
                match stored {
                    Some(object) => (String::from_utf8(object.value).unwrap().parse().unwrap(), Some(object.version)),
                    None => (now, None),
                }
            }
        };
        let plan = self.schedule.plan_catchup(last, now);
        for at in plan.run {
            let run_id = fire_run_id(DEPLOYMENT, SCHEDULE, at);
            match self.write_run(node, &run_id).await {
                Some(Ok(())) => *self.fires.entry(at).or_default() += 1,
                Some(Err(StoreError::Conflict(_))) => {}
                _ => return,
            }
            let value = at.to_string().into_bytes();
            let cas = {
                let store = store.clone();
                async move {
                    match version {
                        Some(version) => store.compare_and_swap(CRON_LAST_KEY, version, value).await,
                        None => store.create_if_absent(CRON_LAST_KEY, value).await,
                    }
                }
            };
            match self.write(node, cas).await {
                Some(Ok(next)) => {
                    last = at;
                    version = Some(next);
                }
                Some(Err(StoreError::Precondition(_) | StoreError::Conflict(_))) => {
                    // Another active moved it: reload next tick.
                    if let State::Active(active) = &mut self.nodes[node].state {
                        active.cron_last = None;
                    }
                    return;
                }
                _ => return,
            }
        }
        if let State::Active(active) = &mut self.nodes[node].state {
            active.cron_last = Some((last, version));
        }
    }

    async fn deliver_webhooks(&mut self) {
        let now = self.time();
        for index in 0..self.deliveries.len() {
            let delivery = &self.deliveries[index];
            if delivery.next_attempt > now || (delivery.acked && !delivery.redeliver) {
                continue;
            }
            let redelivery = delivery.acked;
            let key = delivery.key.clone();
            // The VIP routes to any process that believes it is active.
            let actives: Vec<usize> = (0..self.nodes.len()).filter(|&node| self.nodes[node].is_active()).collect();
            let backoff = self.rng.range(500, 5_000);
            let Some(&target) = actives.get(self.rng.below(actives.len().max(1) as u64) as usize) else {
                self.deliveries[index].next_attempt = now + backoff;
                continue;
            };
            if self.nodes[target].frozen(now) {
                self.deliveries[index].next_attempt = now + backoff;
                continue;
            }
            let run_id = hook_run_id(DEPLOYMENT, "h", &key);
            let outcome = self.write_run(target, &run_id).await;
            let delivery = &mut self.deliveries[index];
            match outcome {
                Some(Ok(())) => {
                    delivery.new_runs += 1;
                    delivery.acked = true;
                }
                Some(Err(StoreError::Conflict(_))) => delivery.acked = true,
                _ => {
                    delivery.next_attempt = now + backoff;
                    continue;
                }
            }
            if redelivery {
                delivery.redeliver = false;
            } else {
                delivery.next_attempt = now + backoff;
            }
        }
    }

    async fn tick(&mut self) {
        self.inject_chaos();
        let now = self.time();
        for node in 0..self.nodes.len() {
            if self.nodes[node].frozen(now) {
                continue;
            }
            if self.nodes[node].frozen_until.is_some() {
                self.resume(node);
            }
            match &self.nodes[node].state {
                State::Down { restart_at } if *restart_at <= now => {
                    let target = &mut self.nodes[node];
                    target.incarnation += 1;
                    target.partitioned_until = None;
                    let candidate = Candidate::new(target.identity(), self.settings);
                    target.state = State::Passive { candidate, next_poll: now };
                }
                State::Down { .. } => {}
                State::Passive { .. } => self.step_passive(node).await,
                State::Active(_) => self.step_active(node).await,
            }
        }
        self.deliver_webhooks().await;
        if now >= CHAOS_END_MS && self.settings.takeover == Takeover::Manual && self.active_index().is_none() {
            // Manual mode: the operator promotes after the incident.
            if now.is_multiple_of(10_000) {
                self.disk.promote = Some("*".into());
            }
        }
    }

    async fn check(&mut self) {
        // I8: tokens of committed writes never go back; one token, one process.
        let mut owner_of: HashMap<u64, (usize, u32)> = HashMap::new();
        let mut highest = 0;
        for (position, &(token, node, incarnation)) in self.commits.clone().iter().enumerate() {
            if token < highest {
                self.violation(format!("I8: commit #{position} with token {token} after token {highest}"));
            }
            highest = highest.max(token);
            if let Some(&owner) = owner_of.get(&token)
                && owner != (node, incarnation)
            {
                self.violation(format!("I8: token {token} committed by {owner:?} and {:?}", (node, incarnation)));
            }
            owner_of.insert(token, (node, incarnation));
        }
        let actives: Vec<usize> = (0..self.nodes.len())
            .filter(|&node| matches!(&self.nodes[node].state, State::Active(active) if !active.fence.is_tripped()))
            .collect();
        if actives.len() != 1 {
            self.violation(format!("end: {} actives", actives.len()));
        }
        let Some(&active) = actives.first() else { return };
        let State::Active(state) = &self.nodes[active].state else { return };
        let store = state.store.clone();
        // I3: at most one run per fire time and no missed fire.
        let fired: Vec<(i64, u32)> = self.fires.iter().map(|(at, count)| (*at, *count)).collect();
        for (at, count) in &fired {
            if *count > 1 {
                self.violation(format!("I3: fire {at} created {count} runs"));
            }
        }
        if let (Some(first), Ok(Some(last))) = (fired.first().map(|(at, _)| *at), store.get_object(CRON_LAST_KEY).await)
        {
            let last: i64 = String::from_utf8(last.value).unwrap().parse().unwrap();
            let mut at = first;
            while at <= last {
                if store.get(&fire_run_id(DEPLOYMENT, SCHEDULE, at)).await.unwrap().is_none() {
                    self.violation(format!("I3: fire {at} missed"));
                }
                at += 60;
            }
            let end_wall = self.wall(active);
            if last < end_wall - 120 {
                self.violation(format!("I3: cron stopped at {last}, wall {end_wall}"));
            }
        }
        // I4: one new run per key, every delivery acknowledged and stored.
        let deliveries: Vec<(String, bool, u32)> =
            self.deliveries.iter().map(|delivery| (delivery.key.clone(), delivery.acked, delivery.new_runs)).collect();
        for (key, acked, new_runs) in deliveries {
            if new_runs > 1 {
                self.violation(format!("I4: key {key} created {new_runs} runs"));
            }
            if !acked {
                self.violation(format!("I4: key {key} never acknowledged"));
            }
            if store.get(&hook_run_id(DEPLOYMENT, "h", &key)).await.unwrap().is_none() && acked {
                self.violation(format!("I4: key {key} acknowledged without a stored run"));
            }
        }
        let takeovers = self.report.takeovers_ms.clone();
        for duration in takeovers {
            if duration > TAKEOVER_BOUND_MS && self.settings.takeover == Takeover::Auto {
                self.violation(format!("takeover took {duration} ms"));
            }
        }
    }
}

async fn run_seed(seed: u64, dir: &std::path::Path) -> Report {
    let mut world = World::new(seed, dir);
    while world.time() < END_MS {
        world.tick().await;
        world.now.fetch_add(TICK_MS, Ordering::Relaxed);
    }
    world.check().await;
    for node in &mut world.nodes {
        node.state = State::Down { restart_at: u64::MAX };
    }
    for suffix in ["", "-wal", "-shm"] {
        let _cleanup = std::fs::remove_file(format!("{}{suffix}", world.path.display()));
    }
    // Manual takeovers wait for the operator: their duration is not bounded.
    if world.settings.takeover == Takeover::Manual {
        world.report.takeovers_ms.clear();
    }
    world.report
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|value| value.parse().ok()).unwrap_or(default)
}

#[test]
fn failover_chaos_keeps_the_invariants() {
    let seeds = env_u64("GRAPHRT_SIM_SEEDS", 1_000);
    let start = env_u64("GRAPHRT_SIM_SEED_START", 0);
    let lanes = env_u64("GRAPHRT_SIM_LANES", 8).max(1);
    let dir = std::env::temp_dir().join(format!("graphrt-gw-failover-sim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let started = std::time::Instant::now();
    let handles: Vec<_> = (0..lanes)
        .map(|lane| {
            let dir = dir.clone();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                let mut reports = Vec::new();
                let mut seed = start + lane;
                while seed < start + seeds {
                    reports.push(runtime.block_on(run_seed(seed, &dir)));
                    seed += lanes;
                }
                reports
            })
        })
        .collect();
    let reports: Vec<Report> = handles.into_iter().flat_map(|handle| handle.join().unwrap()).collect();
    let _cleanup = std::fs::remove_dir_all(&dir);
    let violations: Vec<&String> = reports.iter().flat_map(|report| &report.violations).collect();
    let mut takeovers: Vec<u64> = reports.iter().flat_map(|report| report.takeovers_ms.iter().copied()).collect();
    takeovers.sort_unstable();
    let percentile = |p: usize| takeovers.get((takeovers.len().saturating_sub(1)) * p / 100).copied().unwrap_or(0);
    println!(
        "failover simulation: {seeds} seeds in {:.1} s, {} promotions, {} commits, {} fenced writes refused, takeover (auto, single fault) n={} p50={} ms p99={} ms max={} ms, {} violations",
        started.elapsed().as_secs_f64(),
        reports.iter().map(|report| u64::from(report.promotions)).sum::<u64>(),
        reports.iter().map(|report| report.commits).sum::<u64>(),
        reports.iter().map(|report| report.fenced_writes).sum::<u64>(),
        takeovers.len(),
        percentile(50),
        percentile(99),
        takeovers.last().copied().unwrap_or(0),
        violations.len()
    );
    for violation in violations.iter().take(20) {
        println!("  {violation}");
    }
    assert!(violations.is_empty(), "{} invariant violations", violations.len());
}
