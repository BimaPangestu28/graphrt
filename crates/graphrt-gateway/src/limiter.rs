//! In-memory rate limits on the hot path (no store): GCRA per key (API key,
//! JWT subject) and per client IP.
//!
//! GCRA keeps one timestamp per key (the theoretical arrival time), so it is
//! O(1) and exact. Maps are bounded: when full, expired entries are swept and,
//! if still full, the request is refused (fail closed) rather than growing
//! without bound under an IP-spraying attack.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::Duration;

/// Rate and burst of one limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimit {
    /// Sustained requests per second.
    pub per_second: f64,
    /// Requests allowed at once.
    pub burst: u32,
}

impl RateLimit {
    fn emission_interval_ms(&self) -> f64 {
        1000.0 / self.per_second.max(f64::MIN_POSITIVE)
    }
    fn tolerance_ms(&self) -> f64 {
        self.emission_interval_ms() * f64::from(self.burst.max(1).saturating_sub(1))
    }
}

/// Outcome of a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Go ahead.
    Allow,
    /// Refused; retry after this long.
    Deny(Duration),
}

/// GCRA limiter over keys of type `K`.
#[derive(Debug)]
pub struct Gcra<K> {
    limit: RateLimit,
    max_keys: usize,
    arrivals: Mutex<HashMap<K, f64>>,
}

impl<K: Eq + Hash + Clone> Gcra<K> {
    /// A limiter keeping at most `max_keys` keys.
    pub fn new(limit: RateLimit, max_keys: usize) -> Self {
        Self { limit, max_keys: max_keys.max(1), arrivals: Mutex::new(HashMap::new()) }
    }

    /// Checks one request of `key` at `now_ms` (monotonic milliseconds).
    pub fn check(&self, key: &K, now_ms: u64) -> Decision {
        let now = now_ms as f64;
        let interval = self.limit.emission_interval_ms();
        let tolerance = self.limit.tolerance_ms();
        let mut arrivals = self.arrivals.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !arrivals.contains_key(key) && arrivals.len() >= self.max_keys {
            arrivals.retain(|_, tat| *tat > now);
            if arrivals.len() >= self.max_keys {
                return Decision::Deny(Duration::from_secs(1));
            }
        }
        let tat = arrivals.get(key).copied().unwrap_or(now).max(now);
        let allow_at = tat - tolerance;
        if now < allow_at {
            return Decision::Deny(Duration::from_millis((allow_at - now).ceil() as u64));
        }
        arrivals.insert(key.clone(), tat + interval);
        Decision::Allow
    }

    /// Tracked keys (for `/metrics`).
    pub fn len(&self) -> usize {
        self.arrivals.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
    }

    /// Whether no key is tracked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_burst_then_paces() {
        let limiter = Gcra::new(RateLimit { per_second: 10.0, burst: 3 }, 100);
        let key = "ip".to_owned();
        for _ in 0..3 {
            assert_eq!(limiter.check(&key, 1_000), Decision::Allow);
        }
        assert!(matches!(limiter.check(&key, 1_000), Decision::Deny(wait) if wait == Duration::from_millis(100)));
        assert_eq!(limiter.check(&key, 1_100), Decision::Allow);
        assert!(matches!(limiter.check(&key, 1_100), Decision::Deny(_)));
        assert_eq!(limiter.check(&key, 2_000), Decision::Allow);
    }

    #[test]
    fn keys_are_independent_and_bounded() {
        let limiter = Gcra::new(RateLimit { per_second: 1.0, burst: 1 }, 2);
        assert_eq!(limiter.check(&1, 0), Decision::Allow);
        assert_eq!(limiter.check(&2, 0), Decision::Allow);
        assert!(matches!(limiter.check(&3, 0), Decision::Deny(_)), "full map fails closed");
        assert_eq!(limiter.check(&3, 5_000), Decision::Allow, "expired keys are swept");
    }
}
