//! One tokio task type for many spawn sites.
//!
//! `tokio::spawn` and `tokio::task::spawn_blocking` are generic over the
//! future / closure, and each type compiles its own task machinery (poll,
//! harness, join handle: about 1.9 KiB per call site on x86_64). Stores run
//! every operation on the blocking pool, so a store with 30 methods paid for 30
//! task types. [`spawn`] and [`spawn_blocking`] erase the future / closure
//! behind a box (one allocation, which a spawn makes anyway), so the machinery
//! exists once for each.

use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

type ErasedFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Spawns `future` on the current runtime, like `tokio::spawn`.
pub fn spawn(future: impl Future<Output = ()> + Send + 'static) -> tokio::task::JoinHandle<()> {
    spawn_erased(Box::pin(future))
}

#[inline(never)]
fn spawn_erased(future: ErasedFuture) -> tokio::task::JoinHandle<()> {
    tokio::spawn(future)
}

type ErasedJob = Box<dyn FnOnce() -> Box<dyn Any + Send> + Send>;
type ErasedOutput = Result<Box<dyn Any + Send>, tokio::task::JoinError>;

/// Runs `job` on the blocking pool, like `tokio::task::spawn_blocking`.
///
/// Like an `async fn`, the job is handed to the pool when the returned future
/// is first polled, not when it is created. The future resolves to the
/// `JoinError` when the job panicked or the runtime shut down.
pub fn spawn_blocking<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> Blocking<T> {
    let erased: ErasedJob = Box::new(move || Box::new(job()) as Box<dyn Any + Send>);
    Blocking { job: ErasedBlocking { job: Some(erased), handle: None }, output: PhantomData }
}

/// The future of [`spawn_blocking`].
///
/// An `async fn` here was a state machine per job type around the join handle,
/// inlined into every caller's own state machine; here the spawn and the poll
/// are one non-generic function and only the downcast is per type (about
/// 19 KiB less in the gateway core).
#[must_use = "a blocking job runs only once its future is polled"]
pub struct Blocking<T> {
    job: ErasedBlocking,
    output: PhantomData<fn() -> T>,
}

impl<T> std::fmt::Debug for Blocking<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Blocking").field("spawned", &self.job.handle.is_some()).finish_non_exhaustive()
    }
}

struct ErasedBlocking {
    job: Option<ErasedJob>,
    handle: Option<tokio::task::JoinHandle<Box<dyn Any + Send>>>,
}

impl<T: 'static> Future for Blocking<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let output = match poll_erased(&mut self.get_mut().job, cx) {
            Poll::Ready(output) => output?,
            Poll::Pending => return Poll::Pending,
        };
        // The erased job returns exactly a `T`, so the downcast cannot fail.
        match output.downcast::<T>() {
            Ok(value) => Poll::Ready(Ok(*value)),
            Err(_) => unreachable!("blocking job returned another type"),
        }
    }
}

#[inline(never)]
fn poll_erased(state: &mut ErasedBlocking, cx: &mut Context<'_>) -> Poll<ErasedOutput> {
    if let Some(job) = state.job.take() {
        state.handle = Some(tokio::task::spawn_blocking(job));
    }
    match &mut state.handle {
        Some(handle) => Pin::new(handle).poll(cx),
        // Not reached: the job was spawned just above or by an earlier poll.
        None => Poll::Pending,
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn returns_the_value_and_reports_panics() {
        assert_eq!(super::spawn_blocking(|| 6 * 7).await.unwrap(), 42);
        assert_eq!(super::spawn_blocking(|| String::from("x")).await.unwrap(), "x");
        let panicked = super::spawn_blocking(|| -> u8 { std::panic::panic_any("boom") }).await;
        assert!(panicked.unwrap_err().is_panic());
        let (sender, receiver) = tokio::sync::oneshot::channel();
        super::spawn(async move { sender.send(7).unwrap() }).await.unwrap();
        assert_eq!(receiver.await.unwrap(), 7);
    }

    #[tokio::test]
    async fn the_job_starts_when_first_polled() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        let job = super::spawn_blocking(move || flag.store(true, Ordering::SeqCst));
        // Long enough for the blocking pool to have run a job spawned eagerly.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!ran.load(Ordering::SeqCst), "the job ran before its future was polled");
        job.await.unwrap();
        assert!(ran.load(Ordering::SeqCst));
    }
}
