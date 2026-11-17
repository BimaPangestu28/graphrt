//! Run-wide control state shared by every frame of a run: abort / failure
//! stops, pause requests, external variable updates and pause reasons.
//!
//! Frames wait on [`Control::changed`] alongside their in-flight nodes, so a
//! command takes effect as soon as it is issued instead of when the next node
//! happens to finish.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

use crate::event::PauseReason;
use crate::pool::Selector;
use crate::value::Value;

/// Why a run stopped before its graph drained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// Cancelled by the host.
    Abort(String),
    /// A run-wide limit was exceeded (steps, duration).
    Fail(String),
}

/// Shared control block. All methods are cheap and lock-free on the hot path
/// except for the rarely used update / reason lists.
#[derive(Debug, Default)]
pub(crate) struct Control {
    version: AtomicU64,
    stopped: AtomicBool,
    stop: Mutex<Option<Stop>>,
    pause_requested: AtomicBool,
    reasons: Mutex<Vec<PauseReason>>,
    updates: Mutex<Vec<(Selector, Value)>>,
    wakers: Mutex<Vec<Waker>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock only means another thread panicked while holding it; the
    // data (plain lists) is still usable.
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Control {
    /// Current change counter.
    pub(crate) fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    fn notify(&self) {
        self.version.fetch_add(1, Ordering::AcqRel);
        let wakers: Vec<Waker> = std::mem::take(&mut *lock(&self.wakers));
        for w in wakers {
            w.wake();
        }
    }

    /// Request a stop. The first stop wins.
    pub(crate) fn stop(&self, stop: Stop) {
        {
            let mut slot = lock(&self.stop);
            if slot.is_none() {
                *slot = Some(stop);
            }
        }
        self.stopped.store(true, Ordering::Release);
        self.notify();
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub(crate) fn stop_reason(&self) -> Option<Stop> {
        lock(&self.stop).clone()
    }

    /// Ask every frame to stop scheduling and drain.
    pub(crate) fn request_pause(&self, reason: Option<PauseReason>) {
        if let Some(r) = reason {
            lock(&self.reasons).push(r);
        }
        self.pause_requested.store(true, Ordering::Release);
        self.notify();
    }

    pub(crate) fn pause_requested(&self) -> bool {
        self.pause_requested.load(Ordering::Acquire)
    }

    /// Record why a node paused (reported in `RunPaused`).
    pub(crate) fn add_reason(&self, reason: PauseReason) {
        lock(&self.reasons).push(reason);
    }

    pub(crate) fn reasons(&self) -> Vec<PauseReason> {
        lock(&self.reasons).clone()
    }

    /// Queue an external variable write for the root frame.
    pub(crate) fn push_update(&self, selector: Selector, value: Value) {
        lock(&self.updates).push((selector, value));
        self.notify();
    }

    pub(crate) fn take_updates(&self) -> Vec<(Selector, Value)> {
        std::mem::take(&mut *lock(&self.updates))
    }

    /// Future resolving once the control state changes after `seen`.
    pub(crate) fn changed(&self, seen: u64) -> Changed<'_> {
        Changed { control: self, seen }
    }
}

/// See [`Control::changed`].
#[derive(Debug)]
pub(crate) struct Changed<'a> {
    control: &'a Control,
    seen: u64,
}

impl Future for Changed<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.control.version() != self.seen {
            return Poll::Ready(());
        }
        {
            let mut wakers = lock(&self.control.wakers);
            if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
                wakers.push(cx.waker().clone());
            }
        }
        // Re-check to close the race with a concurrent notify.
        if self.control.version() != self.seen { Poll::Ready(()) } else { Poll::Pending }
    }
}
