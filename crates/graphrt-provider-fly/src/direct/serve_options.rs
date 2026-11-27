//! The Fly side of what a direct deployment turns on in `graphrt serve`: the
//! store volume. The options themselves are shared by every platform
//! ([`graphrt_provider_api::direct`]).

use serde::Serialize;

pub use graphrt_provider_api::direct::{
    ChatAccess, ChatOptions, DirectChannel, STORE_PATH, ServeOptions, ServeSummary,
};

/// Name of the Fly volume that holds a deployment's store.
pub const STORE_VOLUME_NAME: &str = "graphrt_store";
/// Size of a new store volume. Devices, push subscriptions and conversations
/// are small JSON files: the smallest volume Fly offers is plenty.
pub const STORE_VOLUME_GB: u32 = 1;

/// The store volume of a deployment, in deploy results and the status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StoreSummary {
    /// Fly volume id (`vol_...`).
    pub volume: String,
    /// Size in GB (what Fly bills).
    pub size_gb: u32,
    /// Region the volume, and so the Machine, lives in.
    pub region: String,
    /// Whether the serving version uses it. `false`: kept from an earlier
    /// deploy and still billed, until a deploy with `--drop-store` deletes it.
    pub in_use: bool,
}
