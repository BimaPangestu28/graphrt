//! Per-user request rate limit (GCRA, the "virtual scheduling" token bucket).
//!
//! Each user costs one `Instant` (the theoretical arrival time of their next
//! request), so a check is a hash lookup under a short lock. Users whose bucket
//! has fully refilled are dropped when the map grows, which keeps memory bounded
//! by the number of users active within the last minute.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Map size at which refilled entries are first pruned.
const PRUNE_THRESHOLD: usize = 4096;

/// `per_minute` requests per user per minute, with bursts of up to `per_minute`.
#[derive(Debug)]
pub struct UserRateLimiter {
    /// Time one request "costs".
    interval: Duration,
    /// How far ahead of `now` a user's schedule may run (the burst).
    tolerance: Duration,
    state: Mutex<LimiterState>,
}

#[derive(Debug)]
struct LimiterState {
    next_arrival: HashMap<Box<str>, Instant>,
    prune_at: usize,
}

impl UserRateLimiter {
    /// A limiter allowing `per_minute` (at least 1) requests per user per minute.
    pub fn per_minute(per_minute: u32) -> Self {
        let per_minute = per_minute.max(1);
        let interval = Duration::from_secs(60) / per_minute;
        Self {
            interval,
            tolerance: interval * (per_minute - 1),
            state: Mutex::new(LimiterState { next_arrival: HashMap::new(), prune_at: PRUNE_THRESHOLD }),
        }
    }

    /// Count one request of `user` at `now`.
    ///
    /// @returns `Err(wait)` when over the limit: how long until the next request is allowed.
    pub fn check(&self, user: &str, now: Instant) -> Result<(), Duration> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let scheduled = state.next_arrival.get(user).copied().filter(|t| *t > now).unwrap_or(now);
        let ahead = scheduled.duration_since(now);
        if ahead > self.tolerance {
            return Err(ahead - self.tolerance);
        }
        let next = scheduled + self.interval;
        match state.next_arrival.get_mut(user) {
            Some(slot) => *slot = next,
            None => {
                state.next_arrival.insert(user.into(), next);
                if state.next_arrival.len() >= state.prune_at {
                    state.next_arrival.retain(|_, t| *t > now);
                    state.prune_at = (state.next_arrival.len() * 2).max(PRUNE_THRESHOLD);
                }
            }
        }
        Ok(())
    }

    /// Users currently tracked (tests and diagnostics).
    pub fn tracked_users(&self) -> usize {
        self.state.lock().map(|s| s.next_arrival.len()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_a_burst_then_paces_requests() {
        let limiter = UserRateLimiter::per_minute(3);
        let t0 = Instant::now();
        for _ in 0..3 {
            assert_eq!(limiter.check("u", t0), Ok(()));
        }
        let wait = limiter.check("u", t0).unwrap_err();
        assert_eq!(wait, Duration::from_secs(20));
        // Another user has their own bucket.
        assert_eq!(limiter.check("v", t0), Ok(()));
        // One interval later exactly one more request fits.
        let t1 = t0 + Duration::from_secs(20);
        assert_eq!(limiter.check("u", t1), Ok(()));
        assert!(limiter.check("u", t1).is_err());
        // After a full minute of silence the whole burst is back.
        let t2 = t1 + Duration::from_secs(60);
        for _ in 0..3 {
            assert_eq!(limiter.check("u", t2), Ok(()));
        }
    }

    #[test]
    fn refused_requests_do_not_consume_quota() {
        let limiter = UserRateLimiter::per_minute(1);
        let t0 = Instant::now();
        assert!(limiter.check("u", t0).is_ok());
        for _ in 0..10 {
            assert!(limiter.check("u", t0).is_err());
        }
        assert!(limiter.check("u", t0 + Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn prunes_refilled_users() {
        let limiter = UserRateLimiter::per_minute(60);
        let t0 = Instant::now();
        for i in 0..PRUNE_THRESHOLD - 1 {
            limiter.check(&format!("u{i}"), t0).unwrap();
        }
        assert_eq!(limiter.tracked_users(), PRUNE_THRESHOLD - 1);
        limiter.check("late", t0 + Duration::from_secs(5)).unwrap();
        assert_eq!(limiter.tracked_users(), 1);
    }
}
