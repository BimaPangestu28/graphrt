//! Holding a request open until the server is idle (`GRAPHRT_HOLD_UNTIL_IDLE=1`).
//!
//! AWS Lambda freezes an instance as soon as the response of its request
//! ends, and work the request left running (a run resumed by an approval, the
//! answer to a channel message, the final writes of a run to the store) stops
//! with it until the next request, or for good when Lambda drops the instance.
//! With this switch (which `graphrt deploy --target lambda --direct` sets) every
//! response is sent as it is, without `Content-Length`, and its body ends only
//! once no run executes and no background task holds [`Busy`]: the caller has
//! all of the answer at once, and Lambda keeps the instance running until the
//! work is stored.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;
use futures::future::BoxFuture;
use http_body::{Body as HttpBody, Frame};

use super::state::AppState;

/// Environment switch of the hold.
pub const HOLD_ENV: &str = "GRAPHRT_HOLD_UNTIL_IDLE";
/// Longest hold (Lambda stops a request after 15 minutes).
const MAX_HOLD: Duration = Duration::from_secs(14 * 60);
/// How often idleness is checked.
const POLL: Duration = Duration::from_millis(20);
/// Idle this long in a row counts: work a handler spawns starts within it.
const SETTLE: Duration = Duration::from_millis(100);

static BUSY: AtomicUsize = AtomicUsize::new(0);

/// Background work in progress (the server is not idle while one is held).
#[derive(Debug)]
pub struct Busy(());

impl Busy {
    pub fn hold() -> Self {
        BUSY.fetch_add(1, Ordering::AcqRel);
        Self(())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Whether the switch is on.
pub fn enabled() -> bool {
    std::env::var(HOLD_ENV).is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

async fn wait_idle(state: Arc<AppState>) {
    wait_until_idle(|| state.active_runs() == 0, MAX_HOLD).await;
}

/// Waits until `no_runs` and no [`Busy`] is held, [`SETTLE`] in a row, or `max` passed.
async fn wait_until_idle(no_runs: impl Fn() -> bool, max: Duration) {
    let deadline = tokio::time::Instant::now() + max;
    let mut idle_since: Option<tokio::time::Instant> = None;
    while tokio::time::Instant::now() < deadline {
        let idle = no_runs() && BUSY.load(Ordering::Acquire) == 0;
        let now = tokio::time::Instant::now();
        match (idle, idle_since) {
            (true, Some(since)) if now.duration_since(since) >= SETTLE => return,
            (true, None) => idle_since = Some(now),
            (true, Some(_)) => {}
            (false, _) => idle_since = None,
        }
        tokio::time::sleep(POLL).await;
    }
}

/// A body that ends only after the server became idle.
struct HoldBody {
    inner: Body,
    state: Arc<AppState>,
    waiting: Option<BoxFuture<'static, ()>>,
}

impl HttpBody for HoldBody {
    type Data = axum::body::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = &mut *self;
        if this.waiting.is_none() {
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(None) => this.waiting = Some(Box::pin(wait_idle(Arc::clone(&this.state)))),
                other => return other,
            }
        }
        match this.waiting.as_mut().map(|waiting| waiting.as_mut().poll(cx)) {
            Some(Poll::Pending) => Poll::Pending,
            _ => Poll::Ready(None),
        }
    }
}

/// The middleware: `next`'s response, its body ending once the server is idle.
pub async fn hold_until_idle(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    // Health checks answer at once, also while a run executes.
    let health = matches!(request.uri().path(), "/healthz" | "/readyz");
    let response = next.run(request).await;
    if health {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    // A length would let the adapter end the invocation at the last byte.
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::new(HoldBody { inner: body, state, waiting: None }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_hold_ends_once_background_work_is_done_and_stays_done() {
        let busy = Busy::hold();
        let started = tokio::time::Instant::now();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            drop(busy);
        });
        wait_until_idle(|| true, Duration::from_secs(5)).await;
        let waited = started.elapsed();
        release.await.unwrap_or_default();
        assert!(waited >= Duration::from_millis(150) + SETTLE, "{waited:?}");
        assert!(waited < Duration::from_secs(2), "{waited:?}");
    }

    #[tokio::test]
    async fn the_hold_gives_up_at_its_limit() {
        let started = tokio::time::Instant::now();
        wait_until_idle(|| false, Duration::from_millis(100)).await;
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
