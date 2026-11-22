//! Warm-up without waste (plan F4): predictive prewarm from a learned arrival
//! pattern, a keep-warm budget per deployment, and wake-storm protection.
//!
//! - [`ArrivalModel`]: per deployment, 288 five-minute slots of the UTC day,
//!   kept twice (weekdays, and Saturday/Sunday), each an exponentially weighted
//!   probability that the slot sees client traffic (one `f32` per slot,
//!   2.3 KiB; O(1) per arrival, lazily per elapsed slot). `lead_secs` before a
//!   slot starts, the gateway prewarms when traffic in that slot or the next
//!   one is likely enough (`1 − (1 − p₀)(1 − p₁) ≥ threshold` and
//!   `p₀ ≥ threshold / 2`), so a burst that drifts over a slot boundary from
//!   day to day is still caught; the replica is then held through both slots
//!   ([`ArrivalModel::hold`]) even when `idle_timeout` is shorter. With
//!   `alpha = 0.3` and `threshold = 0.6` a slot needs traffic on three days of
//!   its kind in a row before it is prewarmed, and one or two quiet days stop
//!   it again: a conservative learner that never guesses from one burst. The model is memory only (I7: an idle deployment costs no store
//!   operation); a restarted gateway learns again.
//! - [`WarmBudget`]: most idle replica-seconds per UTC day a deployment may keep
//!   warm (prewarmed, `min_replicas`, or the idle tail before `idle_timeout`).
//!   Once spent, prewarms are refused and idle replicas are released at once,
//!   `min_replicas` included, until the next UTC day. Requests still wake
//!   replicas on demand: the budget never refuses a run.
//! - [`WakeGate`]: at most `max_parallel_wakes` provider wakes in flight for
//!   the whole gateway (FIFO beyond that), so a restart, a midnight of crons or
//!   a burst across many deployments does not hit the cloud API all at once.
//!   Prewarms (never on-demand wakes, whose latency a user waits for) are
//!   spread by a random delay below `prewarm_jitter_ms`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use graphrt_provider_api::http::JitterRng;
use serde::Deserialize;
use tokio::sync::{Semaphore, SemaphorePermit};

/// Seconds per slot of the arrival model.
pub const SLOT_SECS: i64 = 300;
const SLOTS_PER_DAY: usize = 86_400 / SLOT_SECS as usize;
const DAY_SECS: u64 = 86_400;
/// 1970-01-01 was a Thursday: weekday index (0 = Monday) of unix day `d` is `(d + 3) % 7`.
const EPOCH_WEEKDAY: i64 = 3;

/// What asked for a prewarm (the label of the prewarm metrics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrewarmSource {
    /// Learned arrival pattern.
    Predictive,
    /// `prewarm_secs` before a cron fire.
    Cron,
    /// `POST /v1/deployments/{d}/warm`.
    Api,
    /// `min_replicas` of a new version.
    MinReplicas,
}

impl PrewarmSource {
    /// Every source, in metric order.
    pub const ALL: [Self; 4] = [Self::Predictive, Self::Cron, Self::Api, Self::MinReplicas];

    /// This source's counter among per-source counters (in [`Self::ALL`] order).
    pub fn counter(self, counters: &mut [u64; 4]) -> &mut u64 {
        let [predictive, cron, api, min_replicas] = counters;
        match self {
            Self::Predictive => predictive,
            Self::Cron => cron,
            Self::Api => api,
            Self::MinReplicas => min_replicas,
        }
    }

    /// This source's value among per-source counters.
    pub fn value(self, counters: [u64; 4]) -> u64 {
        let mut counters = counters;
        *self.counter(&mut counters)
    }

    /// Metric label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Predictive => "predictive",
            Self::Cron => "cron",
            Self::Api => "api",
            Self::MinReplicas => "min_replicas",
        }
    }
}

/// `warm:` of a deployment in `gateway.yaml`.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WarmConfig {
    /// Learn the arrival pattern and prewarm before expected traffic (default off).
    pub predictive: bool,
    /// How long before a predicted slot the prewarm starts (default 60 s; at
    /// least the provider's wake time).
    pub lead_secs: u64,
    /// Score a slot needs to be prewarmed, in `(0, 1]` (default 0.6).
    pub threshold: f32,
    /// Weight of the newest day in a slot's score, in `(0, 1]` (default 0.3).
    pub alpha: f32,
    /// Most idle replica-seconds kept warm per UTC day (default: unlimited).
    pub max_warm_replica_secs_per_day: Option<u64>,
}

impl Default for WarmConfig {
    fn default() -> Self {
        Self { predictive: false, lead_secs: 60, threshold: 0.6, alpha: 0.3, max_warm_replica_secs_per_day: None }
    }
}

impl WarmConfig {
    /// Checks the ranges serde cannot.
    ///
    /// # Errors
    /// A message naming the setting.
    pub fn validate(&self) -> Result<(), String> {
        if !(self.threshold > 0.0 && self.threshold <= 1.0) {
            return Err("warm.threshold must be in (0, 1]".to_owned());
        }
        if !(self.alpha > 0.0 && self.alpha <= 1.0) {
            return Err("warm.alpha must be in (0, 1]".to_owned());
        }
        if self.lead_secs > 3600 {
            return Err("warm.lead_secs must be at most 3600".to_owned());
        }
        Ok(())
    }
}

/// `wake_storm:` of `gateway.yaml` (gateway-wide).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WakeStormConfig {
    /// Provider wakes in flight at once, all deployments together (0 = unlimited; default 16).
    pub max_parallel_wakes: u32,
    /// Upper bound of the random delay of each prewarm (default 1000 ms).
    pub prewarm_jitter_ms: u64,
}

impl Default for WakeStormConfig {
    fn default() -> Self {
        Self { max_parallel_wakes: 16, prewarm_jitter_ms: 1000 }
    }
}

fn slot_of(unix_secs: i64) -> i64 {
    unix_secs.div_euclid(SLOT_SECS)
}

/// (profile, slot of the day): profile 1 for Saturday and Sunday (UTC), else 0.
fn bucket(slot: i64) -> (usize, usize) {
    let day = slot.div_euclid(SLOTS_PER_DAY as i64);
    let weekend = (day + EPOCH_WEEKDAY).rem_euclid(7) >= 5;
    (usize::from(weekend), slot.rem_euclid(SLOTS_PER_DAY as i64) as usize)
}

/// Learned daily arrival pattern of one deployment (see the module docs).
#[derive(Debug, Clone)]
pub struct ArrivalModel {
    scores: Box<[[f32; SLOTS_PER_DAY]; 2]>,
    current_slot: i64,
    current_hit: bool,
    last_prewarmed_slot: i64,
    alpha: f32,
    threshold: f32,
}

impl ArrivalModel {
    /// An empty model (predicts nothing) starting at `unix_secs`.
    pub fn new(unix_secs: i64, alpha: f32, threshold: f32) -> Self {
        Self {
            scores: Box::new([[0.0; SLOTS_PER_DAY]; 2]),
            current_slot: slot_of(unix_secs),
            current_hit: false,
            last_prewarmed_slot: i64::MIN,
            alpha,
            threshold,
        }
    }

    /// Closes the slots before `unix_secs`: the current one with its outcome,
    /// skipped ones as quiet (at most a week of them).
    fn advance(&mut self, unix_secs: i64) {
        let slot = slot_of(unix_secs);
        if slot <= self.current_slot {
            return;
        }
        self.learn(self.current_slot, f32::from(u8::from(self.current_hit)));
        // At most a week of quiet slots: older ones would only decay buckets the last week decayed.
        for quiet in (self.current_slot + 1).max(slot - 7 * SLOTS_PER_DAY as i64)..slot {
            self.learn(quiet, 0.0);
        }
        self.current_slot = slot;
        self.current_hit = false;
    }

    /// Moves the score of `slot` by `alpha` towards `outcome` (1: traffic, 0: quiet).
    fn learn(&mut self, slot: i64, outcome: f32) {
        let (profile, index) = bucket(slot);
        if let Some(score) = self.scores.get_mut(profile).and_then(|day| day.get_mut(index)) {
            *score += self.alpha * (outcome - *score);
        }
    }

    fn score(&self, slot: i64) -> f32 {
        let (profile, index) = bucket(slot);
        self.scores.get(profile).and_then(|day| day.get(index)).copied().unwrap_or_default()
    }

    /// Records client traffic at `unix_secs`.
    pub fn record_arrival(&mut self, unix_secs: i64) {
        self.advance(unix_secs);
        self.current_hit = true;
    }

    /// Whether to prewarm now for the slot `lead_secs` ahead; true once per slot.
    pub fn should_prewarm(&mut self, unix_secs: i64, lead_secs: u64) -> bool {
        self.advance(unix_secs);
        let target = slot_of(unix_secs.saturating_add(lead_secs as i64));
        let first = self.score(target);
        let likely = 1.0 - (1.0 - first) * (1.0 - self.score(target + 1));
        if target == self.last_prewarmed_slot || likely < self.threshold || first < self.threshold / 2.0 {
            return false;
        }
        self.last_prewarmed_slot = target;
        true
    }

    /// How long a predictive prewarm keeps an unused replica: the lead and the two slots it covers.
    pub fn hold(lead_secs: u64) -> Duration {
        Duration::from_secs(lead_secs + 2 * SLOT_SECS as u64)
    }

    /// Score of the slot containing `unix_secs` (for tests and debugging).
    pub fn score_at(&self, unix_secs: i64) -> f32 {
        self.score(slot_of(unix_secs))
    }
}

/// Keep-warm budget of one deployment, shared by the pools of its versions.
#[derive(Debug)]
pub struct WarmBudget {
    limit_ms: u64,
    /// (UTC day, idle replica-milliseconds used that day).
    used: Mutex<(u64, u64)>,
}

impl WarmBudget {
    /// A budget of `replica_secs_per_day`.
    pub fn new(replica_secs_per_day: u64) -> Self {
        Self { limit_ms: replica_secs_per_day.saturating_mul(1000), used: Mutex::new((0, 0)) }
    }

    fn today(&self, unix_secs: u64) -> std::sync::MutexGuard<'_, (u64, u64)> {
        let mut used = self.used.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let day = unix_secs / DAY_SECS;
        if used.0 != day {
            *used = (day, 0);
        }
        used
    }

    /// Adds idle replica-time spent warm.
    pub fn charge(&self, idle: Duration, unix_secs: u64) {
        let mut used = self.today(unix_secs);
        used.1 = used.1.saturating_add(idle.as_millis() as u64);
    }

    /// Whether today's budget is spent.
    pub fn exhausted(&self, unix_secs: u64) -> bool {
        self.today(unix_secs).1 >= self.limit_ms
    }

    /// Idle replica-seconds used today.
    pub fn used_secs(&self, unix_secs: u64) -> u64 {
        self.today(unix_secs).1 / 1000
    }

    /// The daily limit in replica-seconds.
    pub fn limit_secs(&self) -> u64 {
        self.limit_ms / 1000
    }

    /// Seconds until the budget refills (next UTC midnight).
    pub fn refill_in_secs(unix_secs: u64) -> u64 {
        DAY_SECS - unix_secs % DAY_SECS
    }
}

/// Gateway-wide limit on wakes in flight, and the prewarm jitter.
#[derive(Debug)]
pub struct WakeGate {
    permits: Option<Semaphore>,
    jitter: Duration,
    rng: JitterRng,
    in_flight: AtomicU32,
    /// Wakes that had to wait for a permit.
    pub gated: AtomicU64,
    /// Most wakes ever in flight at once.
    pub peak: AtomicU32,
}

/// A wake in flight; dropping it lets the next one go.
#[derive(Debug)]
pub struct WakePermit<'a> {
    gate: &'a WakeGate,
    _permit: Option<SemaphorePermit<'a>>,
}

impl Drop for WakePermit<'_> {
    fn drop(&mut self) {
        self.gate.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl WakeGate {
    /// A gate from the config, with an entropy-seeded jitter.
    pub fn new(config: WakeStormConfig) -> Self {
        Self::seeded(config, JitterRng::from_entropy())
    }

    /// A gate with a given jitter generator (deterministic simulation).
    pub fn seeded(config: WakeStormConfig, rng: JitterRng) -> Self {
        Self {
            permits: (config.max_parallel_wakes > 0).then(|| Semaphore::new(config.max_parallel_wakes as usize)),
            jitter: Duration::from_millis(config.prewarm_jitter_ms),
            rng,
            in_flight: AtomicU32::new(0),
            gated: AtomicU64::new(0),
            peak: AtomicU32::new(0),
        }
    }

    /// Waits for a wake slot.
    pub async fn acquire(&self) -> WakePermit<'_> {
        let permit = match &self.permits {
            Some(permits) => match permits.try_acquire() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    self.gated.fetch_add(1, Ordering::Relaxed);
                    // The semaphore is never closed.
                    permits.acquire().await.ok()
                }
            },
            None => None,
        };
        let now = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(now, Ordering::Relaxed);
        WakePermit { gate: self, _permit: permit }
    }

    /// Wakes in flight.
    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// A random prewarm delay below `prewarm_jitter_ms` and below `cap`.
    pub fn jitter(&self, cap: Duration) -> Duration {
        let bound = self.jitter.min(cap).as_millis() as u64;
        if bound == 0 {
            return Duration::ZERO;
        }
        Duration::from_millis(self.rng.next_u64() % bound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    #[test]
    fn learns_a_daily_slot_after_three_days_and_forgets_it_after_quiet_ones() {
        // 1970-01-05 was a Monday: days 0..5 below are weekdays.
        let nine = 4 * DAY + 9 * 3600;
        let mut model = ArrivalModel::new(4 * DAY, 0.3, 0.6);
        for day in 0..2 {
            model.record_arrival(day * DAY + nine + 30);
            assert!(!model.should_prewarm(day * DAY + nine + DAY - 60, 60), "day {day}: not learned yet");
        }
        model.record_arrival(2 * DAY + nine + 30);
        assert!(model.should_prewarm(3 * DAY + nine - 60, 60), "three days in a row");
        assert!(!model.should_prewarm(3 * DAY + nine - 55, 60), "once per slot");
        assert!(!model.should_prewarm(3 * DAY + nine + 900, 60), "other slots stay cold");
        // Quiet Thursday: Friday is not prewarmed.
        assert!(!model.should_prewarm(4 * DAY + nine - 60, 60));
        assert!(model.score_at(nine) < 0.6);
    }

    #[test]
    fn a_burst_drifting_over_a_slot_boundary_is_still_predicted() {
        let mut model = ArrivalModel::new(4 * DAY, 0.3, 0.6);
        let nine = 4 * DAY + 9 * 3600;
        for day in 0..4 {
            // 08:58, 09:02, 08:58, 09:02: each slot alone only half the days.
            let drift = if day % 2 == 0 { -120 } else { 120 };
            model.record_arrival(day * DAY + nine + drift);
        }
        assert!(model.score_at(nine) < 0.6 && model.score_at(nine - 120) < 0.6);
        assert!(model.should_prewarm(4 * DAY + nine - 300 - 60, 60), "08:55 + 09:00 together");
    }

    #[test]
    fn weekends_have_their_own_profile() {
        let mut model = ArrivalModel::new(4 * DAY, 0.3, 0.6);
        let nine = 4 * DAY + 9 * 3600;
        for day in 0..5 {
            model.record_arrival(day * DAY + nine);
        }
        assert!(!model.should_prewarm(5 * DAY + nine - 60, 60), "Saturday is not a weekday");
        assert!(!model.should_prewarm(6 * DAY + nine - 60, 60), "nor Sunday");
        assert!(model.should_prewarm(7 * DAY + nine - 60, 60), "Monday: the weekend did not decay it");
    }

    #[test]
    fn long_gaps_are_cheap() {
        let mut model = ArrivalModel::new(0, 0.3, 0.6);
        model.record_arrival(10);
        model.record_arrival(365 * DAY);
        assert!(model.score_at(10) < 0.3);
    }

    #[test]
    fn budget_resets_each_utc_day() {
        let budget = WarmBudget::new(60);
        budget.charge(Duration::from_secs(59), 100);
        assert!(!budget.exhausted(100));
        budget.charge(Duration::from_secs(1), 200);
        assert!(budget.exhausted(200));
        assert_eq!(budget.used_secs(200), 60);
        assert!(!budget.exhausted(86_400 + 1), "next day");
        assert_eq!(WarmBudget::refill_in_secs(86_400 - 10), 10);
    }

    #[test]
    fn config_ranges() {
        assert!(WarmConfig::default().validate().is_ok());
        assert!(WarmConfig { threshold: 0.0, ..WarmConfig::default() }.validate().is_err());
        assert!(WarmConfig { alpha: 1.5, ..WarmConfig::default() }.validate().is_err());
        assert!(WarmConfig { lead_secs: 7200, ..WarmConfig::default() }.validate().is_err());
    }

    #[tokio::test]
    async fn gate_bounds_wakes_in_flight() {
        let gate =
            WakeGate::seeded(WakeStormConfig { max_parallel_wakes: 2, prewarm_jitter_ms: 100 }, JitterRng::seeded(1));
        let first = gate.acquire().await;
        let _second = gate.acquire().await;
        assert_eq!(gate.in_flight(), 2);
        let third = tokio::time::timeout(Duration::from_millis(20), gate.acquire()).await;
        assert!(third.is_err(), "third waits");
        drop(first);
        let _third = gate.acquire().await;
        assert_eq!(gate.peak.load(Ordering::Relaxed), 2);
        assert!(gate.gated.load(Ordering::Relaxed) >= 1);
        assert!(gate.jitter(Duration::from_millis(10)) < Duration::from_millis(10));
        assert_eq!(gate.jitter(Duration::ZERO), Duration::ZERO);
    }
}
