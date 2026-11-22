//! Write fencing of the T1 store under active/passive (plan §8, I8).
//!
//! The lease file only decides *who should* be active (liveness). Safety
//! comes from the store itself: the `meta.fence` row holds the token of the
//! current active, promotion raises it (`fence < token` compare-and-set), and
//! every write transaction of a fenced [`crate::store::SqliteStore`] first
//! checks that the row still equals its own token. SQLite serialises write
//! transactions, so once a newer active committed its promotion no write of
//! an older one can commit, whatever the lease file or the clocks say.
//!
//! A second, local guard is the *self-fence*: the active refuses writes when
//! its last confirmed heartbeat is older than `window`, so a process resumed
//! after a long freeze stops writing before a passive can steal the lease
//! (audit D1: liveness judged on the observer's monotonic clock).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Monotonic milliseconds (simulated in tests, `Instant` in production).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A clock counting milliseconds since its creation.
pub fn monotonic_clock() -> Clock {
    let origin = std::time::Instant::now();
    Arc::new(move || origin.elapsed().as_millis() as u64)
}

/// Why a fenced write was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FenceRefusal {
    /// A newer active owns the store (`stored` > ours): fatal, demote.
    Superseded {
        /// Token in the store.
        stored: u64,
        /// Our token.
        ours: u64,
    },
    /// Our heartbeat is not confirmed recently enough: transient.
    Unconfirmed {
        /// Milliseconds since the last confirmation.
        age_ms: u64,
    },
    /// This process gave the lease up (shutdown or demotion).
    Released,
}

impl std::fmt::Display for FenceRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Superseded { stored, ours } => {
                write!(formatter, "fenced: store owned by token {stored}, this gateway holds {ours}")
            }
            Self::Unconfirmed { age_ms } => write!(formatter, "fenced: lease unconfirmed for {age_ms} ms"),
            Self::Released => formatter.write_str("fenced: lease released"),
        }
    }
}

/// The fencing state of one active gateway, shared by its store handle and
/// its heartbeat loop.
pub struct Fence {
    token: AtomicU64,
    confirmed_ms: AtomicU64,
    window_ms: u64,
    clock: Clock,
    tripped: AtomicBool,
    released: AtomicBool,
    notify: tokio::sync::Notify,
}

impl std::fmt::Debug for Fence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Fence")
            .field("token", &self.token())
            .field("window_ms", &self.window_ms)
            .field("tripped", &self.is_tripped())
            .finish_non_exhaustive()
    }
}

impl Fence {
    /// A fence for `token`, confirmed now, refusing writes once the last
    /// confirmation is older than `window_ms`.
    pub fn new(token: u64, window_ms: u64, clock: Clock) -> Self {
        let now = clock();
        Self {
            token: AtomicU64::new(token),
            confirmed_ms: AtomicU64::new(now),
            window_ms,
            clock,
            tripped: AtomicBool::new(false),
            released: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Our token.
    pub fn token(&self) -> u64 {
        self.token.load(Ordering::Acquire)
    }

    /// Replaces our token (promotion found the store ahead of the lease file).
    pub fn set_token(&self, token: u64) {
        self.token.store(token, Ordering::Release);
    }

    /// Records a confirmed heartbeat.
    pub fn confirm(&self) {
        self.confirmed_ms.store((self.clock)(), Ordering::Release);
    }

    /// Checks the local guards (released, tripped, self-fence window).
    ///
    /// # Errors
    /// The refusal.
    pub fn check_local(&self) -> Result<(), FenceRefusal> {
        if self.released.load(Ordering::Acquire) {
            return Err(FenceRefusal::Released);
        }
        if self.is_tripped() {
            return Err(FenceRefusal::Superseded { stored: 0, ours: self.token() });
        }
        let age_ms = (self.clock)().saturating_sub(self.confirmed_ms.load(Ordering::Acquire));
        if age_ms > self.window_ms {
            return Err(FenceRefusal::Unconfirmed { age_ms });
        }
        Ok(())
    }

    /// Checks the token found in the store.
    ///
    /// # Errors
    /// [`FenceRefusal::Superseded`] (and trips the fence) when it is not ours.
    pub fn check_stored(&self, stored: u64) -> Result<(), FenceRefusal> {
        let ours = self.token();
        if stored == ours {
            return Ok(());
        }
        self.trip();
        Err(FenceRefusal::Superseded { stored, ours })
    }

    /// Marks this gateway superseded and wakes [`Fence::tripped`] waiters.
    pub fn trip(&self) {
        self.tripped.store(true, Ordering::Release);
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    /// Stops all writes (graceful release of the lease).
    pub fn release(&self) {
        self.released.store(true, Ordering::Release);
    }

    /// Whether a newer active took over.
    pub fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    /// Resolves once the fence is tripped.
    pub async fn tripped(&self) {
        while !self.is_tripped() {
            self.notify.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_writes_outside_the_confirmation_window_and_after_release() {
        let now = Arc::new(AtomicU64::new(0));
        let reading = now.clone();
        let fence = Fence::new(3, 1_000, Arc::new(move || reading.load(Ordering::Relaxed)));
        assert_eq!(fence.check_local(), Ok(()));
        now.store(1_500, Ordering::Relaxed);
        assert_eq!(fence.check_local(), Err(FenceRefusal::Unconfirmed { age_ms: 1_500 }));
        fence.confirm();
        assert_eq!(fence.check_local(), Ok(()));
        assert_eq!(fence.check_stored(3), Ok(()));
        assert!(!fence.is_tripped());
        assert_eq!(fence.check_stored(4), Err(FenceRefusal::Superseded { stored: 4, ours: 3 }));
        assert!(fence.is_tripped());
        let released = Fence::new(1, 1_000, Arc::new(|| 0));
        released.release();
        assert_eq!(released.check_local(), Err(FenceRefusal::Released));
    }
}
