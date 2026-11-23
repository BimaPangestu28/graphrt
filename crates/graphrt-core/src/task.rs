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
use std::pin::Pin;

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

/// Runs `job` on the blocking pool, like `tokio::task::spawn_blocking`.
///
/// # Errors
/// The `JoinError` when the job panicked or the runtime shut down.
pub async fn spawn_blocking<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let erased: ErasedJob = Box::new(move || Box::new(job()) as Box<dyn Any + Send>);
    let output = run_erased(erased).await?;
    // The erased job returns exactly a `T`, so the downcast cannot fail.
    match output.downcast::<T>() {
        Ok(value) => Ok(*value),
        Err(_) => unreachable!("blocking job returned another type"),
    }
}

#[inline(never)]
fn run_erased(job: ErasedJob) -> tokio::task::JoinHandle<Box<dyn Any + Send>> {
    tokio::task::spawn_blocking(job)
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
}
