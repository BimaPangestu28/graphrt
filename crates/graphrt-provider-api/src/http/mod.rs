//! Shared HTTP layer of the providers: token bucket per provider API, jittered
//! retry, token caching, and (feature `http`) the reqwest-based client.

pub mod bucket;
#[cfg(feature = "http")]
pub mod client;
pub mod retry;
pub mod token;

pub use bucket::TokenBucket;
#[cfg(feature = "http")]
pub use client::{ProviderHttp, StatusClassifier};
pub use retry::{JitterRng, RetryPolicy, parse_retry_after};
pub use token::{AccessToken, REFRESH_MARGIN, SecretString, StaticToken, TokenCache, TokenFetcher, TokenSource};
