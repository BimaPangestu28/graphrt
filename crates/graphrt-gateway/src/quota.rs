//! Per-deployment quotas (plan F1): `max_replicas`, `max_queued` and
//! `max_wakes_per_hour`. The first two are enforced by the pool; this module
//! holds the sliding window of wakes.

use std::collections::VecDeque;
use std::time::Duration;

use serde::Deserialize;
use tokio::time::Instant;

/// Quota settings of one deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Quotas {
    /// Most replicas at once.
    pub max_replicas: u32,
    /// Most requests waiting for a replica; more get `503 Retry-After`.
    pub max_queued: u32,
    /// Most cold wakes per rolling hour (protects the cloud bill from a wake storm).
    pub max_wakes_per_hour: u32,
}

impl Default for Quotas {
    fn default() -> Self {
        Self { max_replicas: 4, max_queued: 256, max_wakes_per_hour: 600 }
    }
}

/// Sliding one-hour window of wake instants.
#[derive(Debug, Default)]
pub struct WakeWindow {
    wakes: VecDeque<Instant>,
}

const WINDOW: Duration = Duration::from_secs(3600);

impl WakeWindow {
    /// Records a wake at `now` if fewer than `limit` happened in the last hour.
    pub fn try_record(&mut self, now: Instant, limit: u32) -> bool {
        while self.wakes.front().is_some_and(|at| now.saturating_duration_since(*at) >= WINDOW) {
            self.wakes.pop_front();
        }
        if self.wakes.len() >= limit as usize {
            return false;
        }
        self.wakes.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn window_slides() {
        let mut window = WakeWindow::default();
        let start = Instant::now();
        assert!(window.try_record(start, 2));
        assert!(window.try_record(start, 2));
        assert!(!window.try_record(start + Duration::from_secs(10), 2));
        assert!(window.try_record(start + WINDOW, 2));
    }
}
