//! Jittered exponential backoff for 429 / 5xx / network errors (C9).

use std::future::Future;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::error::ProviderError;

/// A tiny SplitMix64 generator for jitter. Not cryptographic; seeded from the
/// std `RandomState` by default or from a fixed seed in simulations.
#[derive(Debug)]
pub struct JitterRng {
    state: AtomicU64,
}

impl JitterRng {
    /// A generator with a fixed seed (deterministic simulation).
    pub fn seeded(seed: u64) -> Self {
        Self { state: AtomicU64::new(seed) }
    }

    /// A generator seeded from the process's random hasher keys.
    pub fn from_entropy() -> Self {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(0x9e37_79b9_7f4a_7c15);
        Self::seeded(hasher.finish())
    }

    /// Next 64 random bits.
    pub fn next_u64(&self) -> u64 {
        let mut value =
            self.state.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed).wrapping_add(0x9e37_79b9_7f4a_7c15);
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn next_unit(&self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform duration in `[low, high]`.
    pub fn between(&self, low: Duration, high: Duration) -> Duration {
        if high <= low {
            return low;
        }
        low + (high - low).mul_f64(self.next_unit())
    }
}

/// Retry policy: `max_attempts` tries in total, delay of attempt `n` (from 0)
/// uniformly in `[d/2, d]` with `d = min(base * 2^n, max_delay)` ("equal
/// jitter"), or the server's `Retry-After` when longer (capped at `max_delay`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts including the first one.
    pub max_attempts: u32,
    /// Base delay.
    pub base_delay: Duration,
    /// Upper bound of one delay.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 5, base_delay: Duration::from_millis(200), max_delay: Duration::from_secs(10) }
    }
}

impl RetryPolicy {
    /// Delay before retry number `attempt` (0 = first retry).
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>, rng: &JitterRng) -> Duration {
        let exponential = self.base_delay.saturating_mul(1u32.checked_shl(attempt.min(20)).unwrap_or(u32::MAX));
        let ceiling = exponential.min(self.max_delay);
        let jittered = rng.between(ceiling / 2, ceiling);
        match retry_after {
            Some(hint) => hint.min(self.max_delay).max(jittered),
            None => jittered,
        }
    }

    /// Runs `operation` until it succeeds, fails with a non-retryable kind, or
    /// attempts run out. Returns the last error.
    pub async fn run<T, F, Fut>(&self, rng: &JitterRng, mut operation: F) -> Result<T, ProviderError>
    where
        F: FnMut(u32) -> Fut,
        Fut: Future<Output = Result<T, ProviderError>>,
    {
        let mut attempt = 0;
        loop {
            match operation(attempt).await {
                Ok(value) => return Ok(value),
                Err(error) if error.kind.is_retryable() && attempt + 1 < self.max_attempts => {
                    tokio::time::sleep(self.delay(attempt, error.retry_after, rng)).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// Parses a `Retry-After` header given in seconds (the HTTP-date form is not
/// used by the provider APIs graphrt talks to and is ignored).
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderErrorKind;

    #[test]
    fn delays_grow_with_equal_jitter_and_cap() {
        let policy =
            RetryPolicy { max_attempts: 10, base_delay: Duration::from_millis(100), max_delay: Duration::from_secs(1) };
        let rng = JitterRng::seeded(7);
        for attempt in 0..10 {
            let ceiling = (Duration::from_millis(100) * 2u32.pow(attempt)).min(Duration::from_secs(1));
            let delay = policy.delay(attempt, None, &rng);
            assert!(delay >= ceiling / 2 && delay <= ceiling, "attempt {attempt}: {delay:?}");
        }
    }

    #[test]
    fn retry_after_is_honoured_but_capped() {
        let policy = RetryPolicy::default();
        let rng = JitterRng::seeded(1);
        assert_eq!(policy.delay(0, Some(Duration::from_secs(3)), &rng), Duration::from_secs(3));
        assert_eq!(policy.delay(0, Some(Duration::from_secs(600)), &rng), Duration::from_secs(10));
    }

    #[test]
    fn seeded_rng_is_deterministic() {
        let first = JitterRng::seeded(42);
        let second = JitterRng::seeded(42);
        assert_eq!(
            (0..5).map(|_| first.next_u64()).collect::<Vec<_>>(),
            (0..5).map(|_| second.next_u64()).collect::<Vec<_>>()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retries_only_retryable_kinds() {
        let policy = RetryPolicy::default();
        let rng = JitterRng::seeded(3);
        let mut calls = 0;
        let result: Result<(), _> = policy
            .run(&rng, |_| {
                calls += 1;
                async { Err(ProviderError::fatal("bad request")) }
            })
            .await;
        assert_eq!(result.unwrap_err().kind, ProviderErrorKind::Fatal);
        assert_eq!(calls, 1);

        let mut attempts_seen = Vec::new();
        let result = policy
            .run(&rng, |attempt| {
                attempts_seen.push(attempt);
                async move { if attempt < 2 { Err(ProviderError::rate_limited("slow down")) } else { Ok(attempt) } }
            })
            .await;
        assert_eq!(result.unwrap(), 2);
        assert_eq!(attempts_seen, vec![0, 1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_max_attempts() {
        let policy = RetryPolicy { max_attempts: 3, ..RetryPolicy::default() };
        let rng = JitterRng::seeded(3);
        let mut calls = 0;
        let result: Result<(), _> = policy
            .run(&rng, |_| {
                calls += 1;
                async { Err(ProviderError::transient("503")) }
            })
            .await;
        assert!(result.is_err());
        assert_eq!(calls, 3);
    }

    #[test]
    fn parses_retry_after_seconds() {
        assert_eq!(parse_retry_after(" 7 "), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }
}
