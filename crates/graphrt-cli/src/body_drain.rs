//! Keeps HTTP/1.1 keep-alive connections usable after early rejections.
//!
//! Several responses are sent before the request body is read: `401`/`403` from
//! the API key or `--verify-jwt`, `429` from `--user-rate-limit` or
//! `--max-active-runs`, `415` for a missing JSON content type, `413` once the
//! body limit is hit. hyper then closes the connection without announcing it,
//! and a client that pools connections (Dart's `http.Client`, OkHttp, Python's
//! `http.client`) writes its next request into the dead socket and fails with
//! "Broken pipe".
//!
//! [`drain_unread_body`] reads what is left of the body after the response is
//! built, up to the body limit and for at most [`DRAIN_TIMEOUT`], so the
//! connection stays reusable. A larger or slower body is not read; the response
//! says `Connection: close` instead, so the client opens a new connection.
//! Requests without a body (and bodies a handler read to the end) skip all of it.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{Request, State};
use axum::http::header::{CONNECTION, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use http_body::{Frame, SizeHint};

/// Longest time a rejected request's body is read before the connection is closed instead.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Most bytes read from a body nobody consumed (`--max-body-bytes`).
#[derive(Clone, Copy, Debug)]
pub struct DrainLimit(pub usize);

/// Middleware: after the inner service answered, read the unread rest of the
/// request body (bounded), or mark the response `Connection: close`.
///
/// @param limit - Most bytes to read from an unread body (DrainLimit)
/// @param request - The incoming request (Request)
/// @param next - The inner service (Next)
/// @returns The inner response, with `Connection: close` when the body could not be drained (Response)
pub async fn drain_unread_body(State(limit): State<DrainLimit>, request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    if body.is_end_stream() {
        return next.run(Request::from_parts(parts, body)).await;
    }
    let shared = SharedBody(Arc::new(Mutex::new(body)));
    let mut response = next.run(Request::from_parts(parts, Body::new(shared.clone()))).await;
    if !drain(shared, limit.0).await {
        response.headers_mut().insert(CONNECTION, HeaderValue::from_static("close"));
    }
    response
}

/// Reads `body` to its end; false if it holds more than `max_bytes`, fails, or takes longer than [`DRAIN_TIMEOUT`].
async fn drain(mut body: SharedBody, max_bytes: usize) -> bool {
    if body.size_hint().lower() > max_bytes as u64 {
        return false;
    }
    let reading = async move {
        let mut drained = 0usize;
        while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            let Ok(frame) = frame else { return false };
            drained = drained.saturating_add(frame.data_ref().map_or(0, Bytes::len));
            if drained > max_bytes {
                return false;
            }
        }
        true
    };
    tokio::time::timeout(DRAIN_TIMEOUT, reading).await.unwrap_or(false)
}

/// The request body, shared between the handler and [`drain`], which reads whatever the handler left.
#[derive(Clone)]
struct SharedBody(Arc<Mutex<Body>>);

impl SharedBody {
    fn with_body<T>(&self, use_body: impl FnOnce(&mut Body) -> T) -> T {
        use_body(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl HttpBody for SharedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        self.with_body(|body| Pin::new(body).poll_frame(cx))
    }

    fn is_end_stream(&self) -> bool {
        self.with_body(|body| body.is_end_stream())
    }

    fn size_hint(&self) -> SizeHint {
        self.with_body(|body| body.size_hint())
    }
}
