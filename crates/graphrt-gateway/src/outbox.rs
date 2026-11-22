//! Completion-callback outbox (plan §4, K14): rows are written in the same
//! transaction as the terminal record ([`SqliteStore::finish_with_callback`])
//! and delivered here with exponential backoff, so a crash or restart never
//! loses a callback. Bodies are signed with the deployment's callback secret
//! (`X-Graphrt-Signature: t=..,v1=..`) and carry `Idempotency-Key`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use graphrt_core::store::now_ms;

use crate::metrics::bump;
use crate::store::OutboxEntry;
use crate::supervisor::Gateway;
use crate::webhook::sign_callback;

/// Attempts before a callback is dropped (≈ 2 days of backoff).
pub const MAX_ATTEMPTS: u32 = 16;

/// Delay before retry `attempt` (0-based): 2^attempt s, capped at 1 h.
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.min(12)).min(Duration::from_secs(3600))
}

/// Delivers one entry. Returns whether the receiver accepted it (2xx).
pub async fn deliver(client: &reqwest::Client, entry: &OutboxEntry, secret: Option<&[u8]>) -> bool {
    let mut request = client
        .post(&entry.url)
        .timeout(Duration::from_secs(10))
        .header("content-type", "application/json")
        .header("idempotency-key", format!("{}-{}", entry.run_id, entry.id))
        .header("x-graphrt-delivery", entry.id.to_string())
        .header("x-graphrt-attempt", (entry.attempts + 1).to_string())
        .body(entry.body.clone());
    if let Some(secret) = secret {
        request = request.header("x-graphrt-signature", sign_callback(secret, now_ms() / 1000, entry.body.as_bytes()));
    }
    request.send().await.is_ok_and(|response| response.status().is_success())
}

/// Delivers due callbacks every second until the task is dropped.
/// `secrets` maps deployment → callback signing secret.
pub async fn run_outbox(gateway: Arc<Gateway>, client: reqwest::Client, secrets: HashMap<String, Vec<u8>>) {
    let store = &gateway.store;
    let metrics = &gateway.metrics;
    loop {
        let due = store.due_callbacks(now_ms(), 32).await.unwrap_or_default();
        for entry in due {
            if deliver(&client, &entry, secrets.get(&entry.deployment).map(Vec::as_slice)).await {
                bump(&metrics.callbacks_delivered);
                let _logged = store.complete_callback(entry.id).await.map_err(|error| eprintln!("gateway: outbox: {error}"));
            } else {
                bump(&metrics.callbacks_failed);
                let next = now_ms() + backoff(entry.attempts).as_millis() as u64;
                if store.retry_callback(entry.id, next, MAX_ATTEMPTS).await.unwrap_or(false) {
                    eprintln!("gateway: callback for run {} dropped after {MAX_ATTEMPTS} attempts", entry.run_id);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn backoff_is_capped() {
        assert_eq!(super::backoff(0).as_secs(), 1);
        assert_eq!(super::backoff(5).as_secs(), 32);
        assert_eq!(super::backoff(30).as_secs(), 3600);
    }
}
