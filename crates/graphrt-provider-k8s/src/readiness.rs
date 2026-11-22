//! Readiness of a deployment version from its Service's EndpointSlices:
//! list once, then watch from the list's `resourceVersion`, re-listing when
//! the server answers `410 Gone` (history compacted).

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::time::Instant;

use crate::client::{KubeClient, KubeError, WatchEvent};
use crate::names::encode_query;
use crate::resources::{EndpointSlice, ObjectList, ReadyAddress, SERVICE_NAME_LABEL};

/// Longest single watch request; the server closes it and we resume.
const WATCH_CHUNK: Duration = Duration::from_secs(60);

/// Ready addresses of one Service, kept current from a watch.
#[derive(Debug)]
pub struct ReadinessTracker {
    client: KubeClient,
    collection_path: String,
    selector: String,
    slices: BTreeMap<String, Vec<ReadyAddress>>,
    resource_version: Option<String>,
}

impl ReadinessTracker {
    /// Track the EndpointSlices of `service` in `namespace`.
    pub fn new(client: KubeClient, namespace: &str, service: &str) -> Self {
        Self {
            client,
            collection_path: format!("/apis/discovery.k8s.io/v1/namespaces/{namespace}/endpointslices"),
            selector: encode_query(&format!("{SERVICE_NAME_LABEL}={service}")),
            slices: BTreeMap::new(),
            resource_version: None,
        }
    }

    /// Ready pod addresses, sorted by pod name, without duplicates.
    pub fn ready(&self) -> Vec<ReadyAddress> {
        let mut ready: Vec<ReadyAddress> = self.slices.values().flatten().cloned().collect();
        ready.sort();
        ready.dedup_by(|left, right| left.pod == right.pod);
        ready
    }

    /// Replace the state with a fresh LIST.
    pub async fn relist(&mut self) -> Result<(), KubeError> {
        let path = format!("{}?labelSelector={}", self.collection_path, self.selector);
        let list: ObjectList<EndpointSlice> = self.client.get(&path).await?;
        self.slices =
            list.items.iter().map(|slice| (slice.metadata.name.clone(), slice.ready_addresses())).collect();
        self.resource_version = Some(list.metadata.resource_version);
        Ok(())
    }

    /// Watch until `done(ready)` holds or `deadline` passes.
    ///
    /// @returns `true` when the condition held, `false` on deadline
    pub async fn wait_until<F>(&mut self, deadline: Instant, mut done: F) -> Result<bool, KubeError>
    where
        F: FnMut(&[ReadyAddress]) -> bool,
    {
        if self.resource_version.is_none() {
            self.relist().await?;
        }
        loop {
            if done(&self.ready()) {
                return Ok(true);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            match tokio::time::timeout_at(deadline, self.watch_once(remaining.min(WATCH_CHUNK), &mut done)).await {
                Err(_elapsed) => return Ok(done(&self.ready())),
                Ok(Ok(true)) => return Ok(true),
                Ok(Ok(false)) => {}
                Ok(Err(error)) if is_gone(&error) => self.relist().await?,
                Ok(Err(error)) => return Err(error),
            }
        }
    }

    /// Follow changes forever, calling `on_change` with the full ready set after
    /// each change (the gateway's replica-death detector, conformance C5).
    /// Returns only on an error that a relist cannot fix.
    pub async fn follow<F>(&mut self, mut on_change: F) -> Result<(), KubeError>
    where
        F: FnMut(&[ReadyAddress]),
    {
        self.relist().await?;
        let mut last = self.ready();
        on_change(&last);
        loop {
            let mut changed = |ready: &[ReadyAddress]| {
                if ready != last.as_slice() {
                    last = ready.to_vec();
                    on_change(ready);
                }
                false
            };
            match self.watch_once(WATCH_CHUNK, &mut changed).await {
                Ok(_) => {}
                Err(error) if is_gone(&error) => {
                    self.relist().await?;
                    changed(&self.ready());
                }
                Err(error) if matches!(error, KubeError::Transport { .. }) => {
                    tracing::debug!(%error, "endpointslice watch dropped, resuming");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// One watch request; `true` as soon as `done` holds.
    async fn watch_once<F>(&mut self, timeout: Duration, done: &mut F) -> Result<bool, KubeError>
    where
        F: FnMut(&[ReadyAddress]) -> bool,
    {
        let resource_version = self.resource_version.clone().unwrap_or_default();
        let path = format!(
            "{}?labelSelector={}&watch=1&allowWatchBookmarks=true&resourceVersion={}&timeoutSeconds={}",
            self.collection_path,
            self.selector,
            encode_query(&resource_version),
            timeout.as_secs().max(1)
        );
        let mut stream = self.client.watch(&path, timeout).await?;
        while let Some(event) = stream.next::<serde_json::Value>().await? {
            self.apply(event, &path)?;
            if done(&self.ready()) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn apply(&mut self, event: WatchEvent<serde_json::Value>, path: &str) -> Result<(), KubeError> {
        if event.kind == "ERROR" {
            let code = event.object.get("code").and_then(serde_json::Value::as_u64).unwrap_or(500);
            let message = event.object.get("message").and_then(serde_json::Value::as_str).unwrap_or_default();
            return Err(KubeError::Status {
                method: "WATCH".into(),
                path: path.to_owned(),
                code: u16::try_from(code).unwrap_or(500),
                reason: event.object.get("reason").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned(),
                message: message.to_owned(),
                retry_after: None,
            });
        }
        let slice: EndpointSlice = serde_json::from_value(event.object)
            .map_err(|error| KubeError::Decode { path: path.to_owned(), message: error.to_string() })?;
        if !slice.metadata.resource_version.is_empty() {
            self.resource_version = Some(slice.metadata.resource_version.clone());
        }
        match event.kind.as_str() {
            "ADDED" | "MODIFIED" => {
                self.slices.insert(slice.metadata.name.clone(), slice.ready_addresses());
            }
            "DELETED" => {
                self.slices.remove(&slice.metadata.name);
            }
            _ => {} // BOOKMARK: only the resourceVersion moves.
        }
        Ok(())
    }
}

fn is_gone(error: &KubeError) -> bool {
    error.status_code() == Some(410) || error.reason() == Some("Expired")
}
