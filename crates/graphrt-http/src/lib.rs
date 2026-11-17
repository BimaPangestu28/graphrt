//! # graphrt-http
//!
//! Outbound HTTP for workflows:
//!
//! - [`HttpClient`]: a shared client service enforcing an [`EgressPolicy`]
//!   (SSRF protection, host allow/deny lists, redirect re-validation, response
//!   size caps, no environment proxies).
//! - [`HttpRequestNode`]: the `http-request` node.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use graphrt_core::{Registry, Services};
//! use graphrt_http::{EgressPolicy, HttpClient};
//!
//! let mut registry = Registry::with_builtins();
//! graphrt_http::register(&mut registry);
//! let services = Services::new().with(HttpClient::new(EgressPolicy::default())?);
//! # let _ = (registry, services);
//! # Ok(())
//! # }
//! ```

mod body;
mod client;
mod node;
mod policy;

pub use client::{ClientError, HttpClient};
pub use node::{HttpRequestNode, INLINE_FILE_LIMIT, KIND};
pub use policy::{EgressError, EgressPolicy, is_public};

use std::sync::Arc;

use graphrt_core::node::{Node, Registry};

/// Register the `http-request` node kind.
pub fn register(registry: &mut Registry) {
    registry.register(KIND, |doc| Ok(Arc::new(HttpRequestNode::from_doc(doc)?) as Arc<dyn Node>));
}
