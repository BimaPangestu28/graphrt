//! Token bucket per provider API, so a burst of wakes never exceeds an API
//! quota (Kubernetes API priority, ECS `RunTask` 20/s, Fly 1 req/s per action).

use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

#[derive(Debug)]
struct BucketState {
    tokens: f64,
    refilled_at: Instant,
}

/// A token bucket: `burst` tokens, refilled at `per_second`. Uses tokio's
/// clock, so tests with a paused clock are deterministic.
#[derive(Debug)]
pub struct TokenBucket {
    per_second: f64,
    burst: f64,
    state: Mutex<BucketState>,
}

impl TokenBucket {
    /// A full bucket of `burst` tokens refilled at `per_second`.
    ///
    /// # Panics
    /// When `per_second` is not positive or `burst` is below one token.
    pub fn new(per_second: f64, burst: u32) -> Self {
        assert!(per_second > 0.0, "token bucket rate must be positive");
        assert!(burst >= 1, "token bucket burst must be at least 1");
        let burst = f64::from(burst);
        Self { per_second, burst, state: Mutex::new(BucketState { tokens: burst, refilled_at: Instant::now() }) }
    }

    /// Takes one token if available, otherwise returns how long to wait.
    pub async fn try_acquire(&self) -> Result<(), Duration> {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(state.refilled_at).as_secs_f64();
        state.tokens = (state.tokens + elapsed * self.per_second).min(self.burst);
        state.refilled_at = now;
        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            return Ok(());
        }
        Err(Duration::from_secs_f64((1.0 - state.tokens) / self.per_second))
    }

    /// Waits until a token is available, then takes it.
    pub async fn acquire(&self) {
        while let Err(wait) = self.try_acquire().await {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn burst_then_refill_rate() {
        let bucket = TokenBucket::new(2.0, 3);
        for _ in 0..3 {
            assert!(bucket.try_acquire().await.is_ok());
        }
        let wait = bucket.try_acquire().await.unwrap_err();
        assert_eq!(wait, Duration::from_millis(500));
        let started = Instant::now();
        bucket.acquire().await;
        assert_eq!(started.elapsed(), Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn refill_never_exceeds_burst() {
        let bucket = TokenBucket::new(10.0, 2);
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(bucket.try_acquire().await.is_ok());
        assert!(bucket.try_acquire().await.is_ok());
        assert!(bucket.try_acquire().await.is_err());
    }
}
