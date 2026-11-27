//! Token bucket rate limits.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// One bucket: `capacity` tokens, refilled at `per_second`.
#[derive(Clone, Copy, Debug)]
pub struct Bucket {
    tokens: f64,
    at: Instant,
}

/// A bucket's rule.
#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub capacity: f64,
    pub per_second: f64,
}

impl Rule {
    /// `per_minute` requests a minute, bursts of a quarter minute's worth (at least 10).
    pub fn per_minute(per_minute: u32) -> Self {
        let per_second = f64::from(per_minute) / 60.0;
        Self { capacity: (f64::from(per_minute) / 4.0).max(10.0), per_second }
    }

    /// `per_second` messages a second, bursts of twice that.
    pub fn per_second(per_second: u32) -> Self {
        Self { capacity: f64::from(per_second) * 2.0, per_second: f64::from(per_second) }
    }
}

impl Bucket {
    /// A full bucket.
    pub fn full(rule: Rule) -> Self {
        Self { tokens: rule.capacity, at: Instant::now() }
    }

    /// Take one token, `false` when there is none.
    pub fn take(&mut self, rule: Rule) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * rule.per_second).min(rule.capacity);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Buckets by key (an account, an address), forgotten when full again.
#[derive(Debug)]
pub struct Limiter {
    rule: Rule,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl Limiter {
    pub fn new(rule: Rule) -> Self {
        Self { rule, buckets: Mutex::new(HashMap::new()) }
    }

    /// Take a token of `key`'s bucket.
    pub fn allow(&self, key: &str) -> bool {
        let mut buckets = self.buckets.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if buckets.len() > 50_000 {
            // Forget buckets that refilled, so the map cannot grow without bound.
            let rule = self.rule;
            buckets.retain(|_, bucket| {
                let mut probe = *bucket;
                probe.take(rule);
                probe.tokens + 1.0 < rule.capacity
            });
        }
        let rule = self.rule;
        buckets.entry(key.to_owned()).or_insert_with(|| Bucket::full(rule)).take(rule)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_a_burst_then_refuses() {
        let limiter = Limiter::new(Rule { capacity: 3.0, per_second: 0.001 });
        assert!(limiter.allow("a"));
        assert!(limiter.allow("a"));
        assert!(limiter.allow("a"));
        assert!(!limiter.allow("a"));
        assert!(limiter.allow("b"));
    }

    #[test]
    fn rules_from_rates() {
        let rule = Rule::per_minute(300);
        assert!((rule.per_second - 5.0).abs() < f64::EPSILON);
        assert!((Rule::per_minute(4).capacity - 10.0).abs() < f64::EPSILON);
        assert!((Rule::per_second(60).capacity - 120.0).abs() < f64::EPSILON);
    }
}
