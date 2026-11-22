//! Readiness of a deployment version from its Service's EndpointSlices:
//! list once, then watch from the list's `resourceVersion`, re-listing when
//! the server answers `410 Gone` (history compacted).

use std::collections::BTreeMap;
use std::time::Duration;

use graphrt_provider_api::{ProviderError, ProviderErrorKind};
use tokio::time::Instant;

use crate::client::{KubeClient, WatchEvent, http_status};
use crate::names::encode_query;
use crate::resources::{ReadyAddress, SERVICE_NAME_LABEL, items, name, ready_addresses, resource_version};

/// Longest single watch request; the server closes it and we resume.
const WATCH_CHUNK: Duration = Duration::from_secs(60);
/// A watch that ends this fast without a change is not reopened at once
/// (servers or proxies that close watches early must not be hammered).
const SHORT_WATCH: Duration = Duration::from_secs(1);
const SHORT_WATCH_PAUSE: Duration = Duration::from_millis(50);

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
    pub async fn relist(&mut self) -> Result<(), ProviderError> {
        let path = format!("{}?labelSelector={}", self.collection_path, self.selector);
        let list = self.client.get(&path).await?;
        self.slices = items(&list).iter().map(|slice| (name(slice).to_owned(), ready_addresses(slice))).collect();
        self.resource_version = Some(resource_version(&list).to_owned());
        Ok(())
    }

    /// Watch until `done(ready)` holds or `deadline` passes.
    ///
    /// @returns `true` when the condition held, `false` on deadline
    pub async fn wait_until<F>(&mut self, deadline: Instant, mut done: F) -> Result<bool, ProviderError>
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
            let started = Instant::now();
            match tokio::time::timeout_at(deadline, self.watch_once(remaining.min(WATCH_CHUNK), &mut done)).await {
                Err(_elapsed) => return Ok(done(&self.ready())),
                Ok(Ok(true)) => return Ok(true),
                Ok(Ok(false)) if started.elapsed() < SHORT_WATCH => {
                    tokio::time::sleep_until(deadline.min(Instant::now() + SHORT_WATCH_PAUSE)).await;
                }
                Ok(Ok(false)) => {}
                Ok(Err(error)) if is_gone(&error) => self.relist().await?,
                Ok(Err(error)) => return Err(error),
            }
        }
    }

    /// Follow changes forever, calling `on_change` with the full ready set after
    /// each change (the gateway's replica-death detector, conformance C5).
    /// Returns only on an error that a relist cannot fix.
    pub async fn follow<F>(&mut self, mut on_change: F) -> Result<(), ProviderError>
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
            let started = Instant::now();
            match self.watch_once(WATCH_CHUNK, &mut changed).await {
                Ok(_) if started.elapsed() < SHORT_WATCH => tokio::time::sleep(SHORT_WATCH_PAUSE).await,
                Ok(_) => {}
                Err(error) if is_gone(&error) => {
                    self.relist().await?;
                    changed(&self.ready());
                }
                Err(error) if error.kind == ProviderErrorKind::Transient => {
                    tracing::debug!(%error, "endpointslice watch dropped, resuming");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// One watch request; `true` as soon as `done` holds.
    async fn watch_once<F>(&mut self, timeout: Duration, done: &mut F) -> Result<bool, ProviderError>
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
        while let Some(event) = stream.next().await? {
            self.apply(event, &path)?;
            if done(&self.ready()) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn apply(&mut self, event: WatchEvent, path: &str) -> Result<(), ProviderError> {
        if event.kind == "ERROR" {
            // A Status object; keep the `HTTP <code>:` convention so `http_status` reads it.
            let code = event.object.get("code").and_then(serde_json::Value::as_u64).unwrap_or(500);
            let message = event.object.get("message").and_then(serde_json::Value::as_str).unwrap_or_default();
            let code = u16::try_from(code).unwrap_or(500);
            let kind = ProviderError::kind_for_status(code);
            return Err(ProviderError::new(kind, format!("HTTP {code}: {message} (WATCH {path})")));
        }
        let slice = event.object;
        if !resource_version(&slice).is_empty() {
            self.resource_version = Some(resource_version(&slice).to_owned());
        }
        match event.kind.as_str() {
            "ADDED" | "MODIFIED" => {
                self.slices.insert(name(&slice).to_owned(), ready_addresses(&slice));
            }
            "DELETED" => {
                self.slices.remove(name(&slice));
            }
            _ => {} // BOOKMARK: only the resourceVersion moves.
        }
        Ok(())
    }
}

fn is_gone(error: &ProviderError) -> bool {
    http_status(error) == Some(410)
}
